use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::protocol::{JsonRpcRequest, JsonRpcResponse};
use super::{McpTransport, TransportError};

pub struct StreamableHttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session_id: Option<String>,
    next_id: AtomicU64,
}

impl StreamableHttpTransport {
    pub fn new(url: &str, headers: &HashMap<String, String>) -> Result<Self, String> {
        let mut header_map = HeaderMap::new();
        for (k, v) in headers {
            let name =
                HeaderName::from_bytes(k.as_bytes()).map_err(|e| format!("invalid header name '{}': {}", k, e))?;
            let value = HeaderValue::from_str(v).map_err(|e| format!("invalid header value for '{}': {}", k, e))?;
            header_map.insert(name, value);
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;

        Ok(Self {
            client,
            url: url.to_string(),
            headers: header_map,
            session_id: None,
            next_id: AtomicU64::new(1),
        })
    }

    fn build_headers(&self) -> HeaderMap {
        let mut h = self.headers.clone();
        h.insert("content-type", HeaderValue::from_static("application/json"));
        h.insert(
            "accept",
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(ref sid) = self.session_id
            && let Ok(v) = HeaderValue::from_str(sid)
        {
            h.insert("mcp-session-id", v);
        }
        h
    }

    fn extract_session_id(&mut self, headers: &HeaderMap) {
        if let Some(v) = headers.get("mcp-session-id")
            && let Ok(s) = v.to_str()
        {
            self.session_id = Some(s.to_string());
        }
    }

    async fn parse_sse_response(&self, resp: reqwest::Response) -> Result<serde_json::Value, TransportError> {
        let mut event_stream = resp
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other))
            .eventsource();

        while let Some(event) = event_stream.next().await {
            // The stream broke apart mid-response. Unlike stdio this costs no
            // more than the one request — each is its own connection — but the
            // caller still did not get an answer.
            let event = event.map_err(|e| TransportError::Broken(format!("SSE parse error: {e}")))?;
            if event.event == "message" || event.event.is_empty() {
                let data = event.data.trim();
                if data.is_empty() {
                    continue;
                }
                if let Ok(rpc_resp) = serde_json::from_str::<JsonRpcResponse>(data) {
                    if let Some(err) = rpc_resp.error {
                        return Err(TransportError::Rpc(format!("MCP error {}: {}", err.code, err.message)));
                    }
                    if let Some(result) = rpc_resp.result {
                        return Ok(result);
                    }
                }
            }
        }

        Err(TransportError::Broken(
            "SSE stream ended without a response".to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl McpTransport for StreamableHttpTransport {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params);
        let body = serde_json::to_string(&req).map_err(|e| TransportError::Rpc(e.to_string()))?;

        // Each request is its own HTTP exchange, so unlike stdio a failure here
        // costs only this call — there is no shared stream to fall out of step.
        // It is still reported as broken: something between here and the server
        // is not working, and the registry is better off rebuilding than
        // retrying into it.
        let resp = self
            .client
            .post(&self.url)
            .headers(self.build_headers())
            .body(body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| TransportError::Broken(format!("HTTP request failed: {e}")))?;

        // A status the server chose to send is an answer, not a transport
        // failure — the connection did its job.
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(TransportError::Rpc(format!("HTTP {}: {}", status, text)));
        }

        self.extract_session_id(resp.headers());

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if content_type.contains("text/event-stream") {
            self.parse_sse_response(resp).await
        } else {
            let text = resp
                .text()
                .await
                .map_err(|e| TransportError::Broken(format!("read body: {e}")))?;
            let rpc_resp: JsonRpcResponse = serde_json::from_str(&text)
                .map_err(|e| TransportError::Rpc(format!("parse JSON-RPC response: {e}")))?;
            if let Some(err) = rpc_resp.error {
                return Err(TransportError::Rpc(format!("MCP error {}: {}", err.code, err.message)));
            }
            rpc_resp
                .result
                .ok_or_else(|| TransportError::Rpc("empty result".to_string()))
        }
    }

    async fn notify(&mut self, method: &str, params: Option<serde_json::Value>) -> Result<(), String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or(serde_json::Value::Null),
        });

        let _ = self
            .client
            .post(&self.url)
            .headers(self.build_headers())
            .body(body.to_string())
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;

        Ok(())
    }

    async fn shutdown(&mut self) {
        if self.session_id.is_some() {
            let _ = self
                .client
                .delete(&self.url)
                .headers(self.build_headers())
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
        }
    }
}

use bytes::Bytes;
use http::Method;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    Json(Value),
    /// No producer yet; providers assign `Json` directly. Ported surface.
    #[allow(dead_code)]
    Raw(Bytes),
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Option<RequestBody>,
    pub timeout: Option<Duration>,
}

impl Request {
    pub fn new(method: Method, url: String) -> Self {
        Self {
            method,
            url,
            headers: HeaderMap::new(),
            body: None,
            timeout: None,
        }
    }

    /// Ported builder; providers assign `req.body` directly today.
    #[allow(dead_code)]
    pub fn with_json<T: Serialize>(mut self, body: &T) -> Self {
        self.body = serde_json::to_value(body).ok().map(RequestBody::Json);
        self
    }

    pub fn prepare_body_for_send(&self) -> Result<PreparedRequestBody, String> {
        let mut headers = self.headers.clone();
        match self.body.as_ref() {
            Some(RequestBody::Raw(raw_body)) => Ok(PreparedRequestBody {
                headers,
                body: Some(raw_body.clone()),
            }),
            Some(RequestBody::Json(body)) => {
                let json = serde_json::to_vec(body).map_err(|err| err.to_string())?;
                if !headers.contains_key(http::header::CONTENT_TYPE) {
                    headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
                }
                Ok(PreparedRequestBody {
                    headers,
                    body: Some(Bytes::from(json)),
                })
            }
            None => Ok(PreparedRequestBody { headers, body: None }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRequestBody {
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
}

#[derive(Debug, Clone)]
pub struct Response {
    /// Read by nobody yet: callers get errors via `TransportError::Http`,
    /// which carries its own status. Ported surface.
    #[allow(dead_code)]
    pub status: http::StatusCode,
    #[allow(dead_code)]
    pub headers: HeaderMap,
    pub body: Bytes,
}

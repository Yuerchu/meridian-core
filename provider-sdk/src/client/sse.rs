//! Ported SSE reader, unconsumed: every provider reads `eventsource_stream`
//! directly today. Kept as the other half of the `client` transport surface.
#![allow(dead_code)]

use crate::client::error::StreamError;
use crate::client::transport::ByteStream;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio::time::timeout;

pub fn sse_stream(stream: ByteStream, idle_timeout: Duration, tx: mpsc::Sender<Result<String, StreamError>>) {
    tokio::spawn(async move {
        let mut stream = stream
            .map(|res| res.map_err(|e| StreamError::Stream(e.to_string())))
            .eventsource();

        loop {
            match timeout(idle_timeout, stream.next()).await {
                Ok(Some(Ok(ev))) => {
                    if tx.send(Ok(ev.data.clone())).await.is_err() {
                        return;
                    }
                }
                Ok(Some(Err(e))) => {
                    let _ = tx.send(Err(StreamError::Stream(e.to_string()))).await;
                    return;
                }
                Ok(None) => return,
                Err(_) => {
                    let _ = tx.send(Err(StreamError::Timeout)).await;
                    return;
                }
            }
        }
    });
}

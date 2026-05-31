//! Transport abstraction for the client connection.
//!
//! A [`Transport`] hides framing and (de)serialization behind a tiny typed
//! interface: the session handler only ever sees [`ClientMessage`] in and
//! [`ServerMessage`] out, so it stays agnostic to the underlying wire (today
//! WebSocket; tomorrow a Unix domain socket or anything else).

use crate::wire::{ClientMessage, ServerMessage};
use axum::extract::ws::{Message, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use std::future::Future;
use tokio::sync::Mutex;
use tracing::warn;

/// A bidirectional, message-framed channel to a single client.
///
/// `recv`/`send` take `&self` so a caller can await an inbound message and emit
/// an outbound one concurrently (e.g. inside a `tokio::select!`).
pub trait Transport {
    /// The next client command, or `None` once the connection is closed.
    /// Malformed or non-data frames are logged and skipped internally.
    fn recv(&self) -> impl Future<Output = Option<ClientMessage>> + Send;

    /// Send a server message. Returns `false` once the connection is gone and
    /// the caller should tear down. A serialization failure is logged but
    /// treated as non-fatal (`true`).
    fn send(&self, msg: &ServerMessage) -> impl Future<Output = bool> + Send;
}

/// [`Transport`] over an axum WebSocket. The split halves are each behind their
/// own mutex so reads and writes proceed independently.
pub struct WebSocketTransport {
    sink: Mutex<SplitSink<WebSocket, Message>>,
    stream: Mutex<SplitStream<WebSocket>>,
}

impl WebSocketTransport {
    pub fn new(socket: WebSocket) -> Self {
        let (sink, stream) = socket.split();
        Self {
            sink: Mutex::new(sink),
            stream: Mutex::new(stream),
        }
    }
}

impl Transport for WebSocketTransport {
    async fn recv(&self) -> Option<ClientMessage> {
        let mut stream = self.stream.lock().await;
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ClientMessage>(&text) {
                        Ok(msg) => return Some(msg),
                        Err(e) => warn!("ignoring malformed client message: {e}"),
                    }
                }
                Some(Ok(Message::Close(_))) | None => return None,
                Some(Ok(_)) => {} // ping/pong/binary — ignore
                Some(Err(e)) => {
                    warn!("websocket read error: {e}");
                    return None;
                }
            }
        }
    }

    async fn send(&self, msg: &ServerMessage) -> bool {
        let json = match serde_json::to_string(msg) {
            Ok(j) => j,
            Err(e) => {
                warn!("failed to serialize server message: {e}");
                return true;
            }
        };
        let mut sink = self.sink.lock().await;
        match sink.send(Message::Text(json.into())).await {
            Ok(()) => true,
            Err(e) => {
                warn!("websocket send error: {e}");
                false
            }
        }
    }
}

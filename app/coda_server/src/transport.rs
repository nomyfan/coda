//! Transport abstraction for the client connection.
//!
//! A [`Transport`] hides connection setup, framing, and (de)serialization
//! behind a tiny typed interface. Each side names what it receives and what it
//! sends via the `Incoming`/`Outgoing` associated types (the server takes
//! `ClientMessage` in, `ServerMessage` out). Today the wire is WebSocket; a
//! future Unix-domain-socket transport can plug in by implementing this trait.

use crate::wire::{ClientMessage, ServerMessage};
use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use std::future::Future;
use tokio::sync::Mutex;
use tracing::warn;

/// A bidirectional, message-framed channel to the peer.
///
/// `recv`/`send` take `&self` so a caller can await an inbound message and emit
/// an outbound one concurrently (e.g. inside a `tokio::select!`).
pub trait Transport {
    /// Messages received from the peer.
    type Incoming;
    /// Messages sent to the peer.
    type Outgoing;

    /// The next inbound message, or `None` once the connection is closed.
    /// Malformed or non-data frames are logged and skipped internally.
    fn recv(&self) -> impl Future<Output = Option<Self::Incoming>> + Send;

    /// Send a message. Returns `false` once the message cannot be delivered and
    /// the caller should tear down.
    fn send(&self, msg: &Self::Outgoing) -> impl Future<Output = bool> + Send;
}

/// Decode a text frame into `T`, logging and yielding `None` on malformed input.
fn decode<T: DeserializeOwned>(text: &str) -> Option<T> {
    match serde_json::from_str(text) {
        Ok(msg) => Some(msg),
        Err(e) => {
            warn!("ignoring malformed message: {e}");
            None
        }
    }
}

/// [`Transport`] over an axum WebSocket (server side). The split halves are each
/// behind their own mutex so reads and writes proceed independently.
pub struct WebSocketTransport {
    sink: Mutex<SplitSink<WebSocket, AxumMessage>>,
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
    type Incoming = ClientMessage;
    type Outgoing = ServerMessage;

    async fn recv(&self) -> Option<ClientMessage> {
        let mut stream = self.stream.lock().await;
        loop {
            match stream.next().await {
                Some(Ok(AxumMessage::Text(text))) => {
                    if let Some(msg) = decode(&text) {
                        return Some(msg);
                    }
                }
                Some(Ok(AxumMessage::Close(_))) | None => return None,
                Some(Ok(_)) => {} // ping/pong/binary: ignore
                Some(Err(e)) => {
                    warn!("websocket read error: {e}");
                    return None;
                }
            }
        }
    }

    async fn send(&self, msg: &ServerMessage) -> bool {
        send_text(&self.sink, msg, |t| AxumMessage::Text(t.into())).await
    }
}

/// Serialize `msg` to a text frame (via `wrap`) and send it over `sink`.
/// Shared by both WebSocket impls, which differ only in their `Message` type.
async fn send_text<M, S, T>(sink: &Mutex<S>, msg: &T, wrap: impl Fn(String) -> M) -> bool
where
    T: Serialize,
    S: SinkExt<M> + Unpin,
    <S as futures::Sink<M>>::Error: Debug,
{
    let json = match serde_json::to_string(msg) {
        Ok(j) => j,
        Err(e) => {
            warn!("failed to serialize message: {e}");
            return false;
        }
    };
    let mut sink = sink.lock().await;
    match sink.send(wrap(json)).await {
        Ok(()) => true,
        Err(e) => {
            warn!("websocket send error: {e:?}");
            false
        }
    }
}

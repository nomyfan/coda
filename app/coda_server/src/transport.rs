//! Transport abstraction for the client connection.
//!
//! A [`Transport`] hides connection setup and framing behind a tiny interface:
//! `recv` hands up the **raw frame text** and `send` serializes a built
//! [`RpcOutgoing`] envelope. The decode/encode asymmetry is deliberate — only
//! the `rpc` layer can turn a malformed frame into an error *response*, so
//! classification lives there, not here (this layer must not silently drop a bad
//! frame the way a typed decode would). Today the wire is WebSocket; a future
//! Unix-domain-socket transport can plug in by implementing this trait.

use crate::rpc::RpcOutgoing;
use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use std::fmt::Debug;
use std::future::Future;
use tokio::sync::Mutex;
use tracing::warn;

/// A bidirectional, text-framed channel to the peer.
///
/// `recv`/`send` take `&self` so a caller can await an inbound frame and emit an
/// outbound one concurrently (e.g. inside a `tokio::select!`).
pub trait Transport {
    /// The next inbound frame's raw text, or `None` once the connection is
    /// closed. Non-data frames (ping/pong/binary) are skipped internally;
    /// malformed *content* is handed up verbatim for the `rpc` layer to classify.
    fn recv(&self) -> impl Future<Output = Option<String>> + Send;

    /// Send a built envelope. Returns `false` once the frame cannot be delivered
    /// and the caller should tear down.
    fn send(&self, msg: &RpcOutgoing) -> impl Future<Output = bool> + Send;
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
    async fn recv(&self) -> Option<String> {
        let mut stream = self.stream.lock().await;
        loop {
            match stream.next().await {
                Some(Ok(AxumMessage::Text(text))) => return Some(text.to_string()),
                Some(Ok(AxumMessage::Close(_))) | None => return None,
                Some(Ok(_)) => {} // ping/pong/binary: ignore
                Some(Err(e)) => {
                    warn!("websocket read error: {e}");
                    return None;
                }
            }
        }
    }

    async fn send(&self, msg: &RpcOutgoing) -> bool {
        send_text(&self.sink, msg, |t| AxumMessage::Text(t.into())).await
    }
}

/// Serialize `msg` to a text frame (via `wrap`) and send it over `sink`.
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

//! Transport abstraction for the client connection.
//!
//! A [`Transport`] hides connection setup and framing behind a tiny interface:
//! `recv` hands up the **raw frame text** and `send` serializes a built
//! [`RpcOutgoing`] envelope. The decode/encode asymmetry is deliberate — only
//! the `rpc` layer can turn a malformed frame into an error *response*, so
//! classification lives there, not here (this layer must not silently drop a bad
//! frame the way a typed decode would). Today the wire is WebSocket; a future
//! Unix-domain-socket transport can plug in by implementing this trait.
//!
//! Keepalive is deliberately *not* part of the trait: it belongs to one wire
//! rather than to the abstraction (a Unix socket has no such concept).
//! [`WebSocketTransport`] pings from inside `recv` instead, so no caller ever
//! sees a control frame.

use crate::config::HeartbeatConfig;
use crate::rpc::RpcOutgoing;
use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use std::fmt::Debug;
use std::future::Future;
use tokio::sync::Mutex;
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at};
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

/// Everything `recv` touches, behind one lock: the read half and its ping timer.
struct Reader {
    stream: SplitStream<WebSocket>,
    /// Held here rather than created inside `recv` because the caller's
    /// `select!` drops the `recv` future whenever another branch wins; a
    /// per-call timer would restart from zero each time and, on a connection
    /// with steady event traffic, never fire.
    ticker: Interval,
}

/// [`Transport`] over an axum WebSocket (server side). The split halves are each
/// behind their own mutex so reads and writes proceed independently.
pub struct WebSocketTransport {
    sink: Mutex<SplitSink<WebSocket, AxumMessage>>,
    reader: Mutex<Reader>,
}

impl WebSocketTransport {
    pub fn new(socket: WebSocket, heartbeat: HeartbeatConfig) -> Self {
        let (sink, stream) = socket.split();
        // `interval` fires its first tick immediately, which would ping a
        // connection that just opened; start one interval out instead.
        let mut ticker = interval_at(Instant::now() + heartbeat.interval, heartbeat.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            sink: Mutex::new(sink),
            reader: Mutex::new(Reader { stream, ticker }),
        }
    }

    /// Send a protocol-level Ping. Fire-and-forget — nothing waits for the Pong,
    /// which browsers send on their own. `false` means the frame could not be
    /// delivered and the caller should tear down, same as [`Transport::send`].
    async fn ping(&self) -> bool {
        let mut sink = self.sink.lock().await;
        match sink.send(AxumMessage::Ping(Default::default())).await {
            Ok(()) => true,
            Err(e) => {
                warn!("websocket ping error: {e:?}");
                false
            }
        }
    }
}

impl Transport for WebSocketTransport {
    async fn recv(&self) -> Option<String> {
        let mut reader = self.reader.lock().await;
        let Reader { stream, ticker } = &mut *reader;
        loop {
            tokio::select! {
                frame = stream.next() => match frame {
                    Some(Ok(AxumMessage::Text(text))) => return Some(text.to_string()),
                    Some(Ok(AxumMessage::Close(_))) | None => return None,
                    // ping/pong/binary: ignore. An inbound Ping is answered by
                    // the underlying tungstenite stream, not by anything here.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        warn!("websocket read error: {e}");
                        return None;
                    }
                },
                // Takes the sink lock while holding the reader lock. The order
                // is only ever reader → sink (`send` takes the sink on its
                // own), so it cannot deadlock.
                _ = ticker.tick() => if !self.ping().await {
                    return None;
                },
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

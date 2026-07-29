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
//! Keepalive is deliberately *not* part of the trait: it is a property of one
//! particular wire rather than of the abstraction (a Unix socket has no such
//! concept). [`WebSocketTransport`] drives its own heartbeat from inside `recv`
//! instead, so neither the `rpc` layer nor the connection loop ever sees a
//! control frame.

use crate::config::HeartbeatConfig;
use crate::rpc::RpcOutgoing;
use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use std::fmt::Debug;
use std::future::Future;
use std::time::Duration;
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
    ///
    /// `None` means "this connection is over": the peer closed it, it errored,
    /// or the transport gave up on a peer that stopped answering. Callers treat
    /// all three alike, so the distinction is not surfaced.
    fn recv(&self) -> impl Future<Output = Option<String>> + Send;

    /// Send a built envelope. Returns `false` once the frame cannot be delivered
    /// and the caller should tear down.
    fn send(&self, msg: &RpcOutgoing) -> impl Future<Output = bool> + Send;
}

/// Everything `recv` touches, behind one lock: the read half plus the heartbeat
/// state it drives.
struct Reader {
    stream: SplitStream<WebSocket>,
    /// Fires every `HeartbeatConfig::interval`. It is held here rather than
    /// created inside `recv` because the caller's `select!` drops the `recv`
    /// future whenever another branch wins; a per-call timer would restart from
    /// zero each time and, on a connection with steady event traffic, never fire.
    ticker: Interval,
    /// When the peer last proved it was alive. *Any* inbound frame counts — a
    /// Pong, but equally a request the client sent on its own.
    last_seen: Instant,
    /// How long `last_seen` may go stale before the peer is declared gone.
    timeout: Duration,
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
        // connection that just opened; start one interval out instead. `Delay`
        // keeps missed ticks from piling up into a burst if `recv` goes
        // unpolled while the caller handles a slow frame.
        let mut ticker = interval_at(Instant::now() + heartbeat.interval, heartbeat.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            sink: Mutex::new(sink),
            reader: Mutex::new(Reader {
                stream,
                ticker,
                last_seen: Instant::now(),
                timeout: heartbeat.timeout(),
            }),
        }
    }

    /// Send a protocol-level Ping. Fire-and-forget: the Pong comes back through
    /// `recv` (browsers answer automatically, with no client-side code), and
    /// nothing here waits for it. `false` means the frame could not be delivered
    /// and the caller should tear down, same as [`Transport::send`].
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
        let Reader {
            stream,
            ticker,
            last_seen,
            timeout,
        } = &mut *reader;
        loop {
            tokio::select! {
                frame = stream.next() => match frame {
                    Some(Ok(AxumMessage::Text(text))) => {
                        *last_seen = Instant::now();
                        return Some(text.to_string());
                    }
                    Some(Ok(AxumMessage::Close(_))) | None => return None,
                    // ping/pong/binary carry nothing to hand up, but they do
                    // prove the peer is there. An *inbound* Ping is answered
                    // with a Pong by the underlying tungstenite stream when it
                    // is polled, not by anything here.
                    Some(Ok(_)) => *last_seen = Instant::now(),
                    Some(Err(e)) => {
                        warn!("websocket read error: {e}");
                        return None;
                    }
                },
                _ = ticker.tick() => {
                    if last_seen.elapsed() > *timeout {
                        warn!("websocket heartbeat timed out; closing connection");
                        return None;
                    }
                    // Takes the sink lock while holding the reader lock. That
                    // order is only ever reader → sink (`send` takes the sink
                    // on its own), so it cannot deadlock; a concurrent `send`
                    // merely stalls this ping, and the next read with it.
                    if !self.ping().await {
                        return None;
                    }
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

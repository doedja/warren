//! yamux multiplexing glue for the node<->hub link.
//!
//! One connection (TLS or plain) carries the control channel plus one logical
//! stream per proxied request, replacing the old conn-per-request + warm-pool
//! design. Opening a stream is cheap (no TCP/TLS handshake), so the warm pool
//! is gone.
//!
//! The node opens streams (control stream first, then one per Dial); the hub
//! only accepts them. yamux 0.13 has no `Control` handle and a `Connection` can
//! only be polled from one place, so each side runs a single driver task that
//! owns the `Connection`: the hub forwards inbound streams over a channel, the
//! node serves outbound-open requests over a channel.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::future::poll_fn;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use yamux::{Config, Connection, Mode};

use crate::conn::Conn;

/// A logical stream over the multiplexed connection, exposed with tokio's
/// AsyncRead/AsyncWrite via the compat shim. Used wherever a per-request
/// connection used to be (control stream and each data stream).
pub type MuxStream = Compat<yamux::Stream>;

/// Handle to request new outbound streams from the node-side driver.
pub type Opener = mpsc::Sender<oneshot::Sender<MuxStream>>;

/// Backlog of the channel carrying streams to/from a driver task.
const STREAM_CHANNEL_CAP: usize = 32;

/// Dead-peer read deadline shared by both control loops: no traffic for this
/// long (~3 missed 20s pings) means the peer is gone, tear the link down.
pub const LINK_READ_TIMEOUT: Duration = Duration::from_secs(75);

fn config() -> Config {
    // Defaults are sized for proxy splices; per-stream flow control is what
    // keeps one slow target from stalling the others on the shared connection.
    Config::default()
}

/// RAII guard for an in-flight counter: increments on construction, decrements
/// on drop, so the count stays correct even if the holding task panics. Shared
/// by the hub (per-node dial cap) and the node (drain accounting).
pub struct InFlightGuard(Arc<AtomicU32>);
impl InFlightGuard {
    pub fn new(c: Arc<AtomicU32>) -> Self {
        c.fetch_add(1, Ordering::Relaxed);
        InFlightGuard(c)
    }
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Owns a driver task and aborts it on drop, so the driver (and the yamux
/// connection it holds) cannot outlive its owner on any return path.
pub struct DriverHandle(tokio::task::JoinHandle<()>);
impl Drop for DriverHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Open a new outbound stream via the node-side driver. Errors if the driver or
/// the underlying connection is gone.
pub async fn open(opener: &Opener) -> Result<MuxStream> {
    let (tx, rx) = oneshot::channel();
    opener
        .send(tx)
        .await
        .map_err(|_| anyhow!("mux driver gone"))?;
    rx.await.map_err(|_| anyhow!("mux stream open failed"))
}

/// Hub side: wrap an accepted node connection as a yamux server and spawn a
/// driver that forwards each inbound stream over the returned channel. The
/// driver MUST keep running for the connection to make progress; the returned
/// [`DriverHandle`] aborts it when dropped.
pub fn server(conn: Conn) -> (mpsc::Receiver<MuxStream>, DriverHandle) {
    let mut yconn: Connection<Compat<Conn>> =
        Connection::new(conn.compat(), config(), Mode::Server);
    let (tx, rx) = mpsc::channel::<MuxStream>(STREAM_CHANNEL_CAP);
    let handle = tokio::spawn(async move {
        // Ends on Some(Err)/None (connection closed) or when the receiver drops.
        while let Some(Ok(s)) = poll_fn(|cx| yconn.poll_next_inbound(cx)).await {
            if tx.send(s.compat()).await.is_err() {
                break;
            }
        }
    });
    (rx, DriverHandle(handle))
}

/// Node side: wrap the dialed connection as a yamux client and spawn a driver
/// that serves outbound-open requests. Returns the [`Opener`] plus a
/// [`DriverHandle`] that aborts the driver when dropped.
pub fn client(conn: Conn) -> (Opener, DriverHandle) {
    let mut yconn: Connection<Compat<Conn>> =
        Connection::new(conn.compat(), config(), Mode::Client);
    let (open_tx, mut open_rx) = mpsc::channel::<oneshot::Sender<MuxStream>>(STREAM_CHANNEL_CAP);
    let handle = tokio::spawn(async move {
        // At most one open is in flight at a time; a yamux open is a local frame
        // write (not a round trip), so this is fast and keeps the single
        // &mut Connection borrow simple even under the hub's dial concurrency cap.
        let mut pending: Option<oneshot::Sender<MuxStream>> = None;
        loop {
            enum Ev {
                Opened(yamux::Result<yamux::Stream>),
                OpenReq(oneshot::Sender<MuxStream>),
                Inbound,
                Done,
            }
            let ev = poll_fn(|cx| {
                if pending.is_some() {
                    if let Poll::Ready(res) = yconn.poll_new_outbound(cx) {
                        return Poll::Ready(Ev::Opened(res));
                    }
                }
                // Drive the connection. The node never expects inbound streams;
                // if one shows up, drop it (returned as Inbound) and keep going.
                match yconn.poll_next_inbound(cx) {
                    Poll::Ready(Some(Ok(_))) => return Poll::Ready(Ev::Inbound),
                    Poll::Ready(Some(Err(_))) | Poll::Ready(None) => return Poll::Ready(Ev::Done),
                    Poll::Pending => {}
                }
                if pending.is_none() {
                    match open_rx.poll_recv(cx) {
                        Poll::Ready(Some(r)) => return Poll::Ready(Ev::OpenReq(r)),
                        Poll::Ready(None) => return Poll::Ready(Ev::Done),
                        Poll::Pending => {}
                    }
                }
                Poll::Pending
            })
            .await;
            match ev {
                Ev::OpenReq(r) => pending = Some(r),
                Ev::Opened(Ok(s)) => {
                    if let Some(r) = pending.take() {
                        // Receiver may have given up; dropping the stream closes it.
                        let _ = r.send(s.compat());
                    }
                }
                // Open failed: drop the responder so the waiter's open() errors.
                Ev::Opened(Err(_)) => pending = None,
                Ev::Inbound => {}
                Ev::Done => break,
            }
        }
    });
    (open_tx, DriverHandle(handle))
}

//! Per-server listen adapter for routing connections from the central
//! `QuicListeners::accept()` loop to individual per-server consumers.
//!
//! The root process runs a single `QuicListeners` that multiplexes all servers.
//! Each server gets a `PerServerListenAdapter` backed by an mpsc channel — the
//! central accept loop routes connections by `server_name` to the appropriate
//! channel sender, and this adapter reads from the receiver.

use std::fmt;

use futures::future::BoxFuture;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

/// Error type for [`PerServerListenAdapter`].
///
/// Implements `std::error::Error + std::any::Any` as required by
/// [`h3x::quic::Listen::Error`].
#[derive(Debug)]
pub enum PerServerListenError {
    /// The mpsc channel was closed (server removed or root shutting down).
    ChannelClosed,
    /// The adapter was explicitly shut down via [`PerServerListenAdapter::shutdown`].
    Shutdown,
}

impl fmt::Display for PerServerListenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelClosed => write!(f, "per-server listen channel closed"),
            Self::Shutdown => write!(f, "per-server listener shut down"),
        }
    }
}

impl std::error::Error for PerServerListenError {}

/// Per-server listener adapter.
///
/// Root creates one per `server_name`, routes connections from the central
/// `QuicListeners::accept()` loop to this adapter's mpsc channel. Wraps the
/// receiver side so it implements [`h3x::quic::Listen`].
pub struct PerServerListenAdapter {
    rx: Mutex<mpsc::Receiver<gm_quic::prelude::Connection>>,
    shutdown_token: CancellationToken,
}

impl PerServerListenAdapter {
    /// Create a new per-server listen adapter.
    ///
    /// * `rx` — receives connections routed by server_name from the central accept loop
    /// * `shutdown_token` — signals shutdown of this adapter
    pub fn new(rx: mpsc::Receiver<gm_quic::prelude::Connection>, shutdown_token: CancellationToken) -> Self {
        Self {
            rx: Mutex::new(rx),
            shutdown_token,
        }
    }
}

impl h3x::quic::Listen for PerServerListenAdapter {
    type Connection = gm_quic::prelude::Connection;
    type Error = PerServerListenError;

    fn accept(&self) -> BoxFuture<'_, Result<Self::Connection, Self::Error>> {
        Box::pin(async {
            let mut rx = self.rx.lock().await;
            tokio::select! {
                conn = rx.recv() => {
                    conn.ok_or(PerServerListenError::ChannelClosed)
                }
                _ = self.shutdown_token.cancelled() => {
                    Err(PerServerListenError::Shutdown)
                }
            }
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, Result<(), Self::Error>> {
        self.shutdown_token.cancel();
        Box::pin(std::future::ready(Ok(())))
    }
}

//! Per-server listen adapter for routing connections from a
//! [`h3x::dquic::ServerBinding`] drain task to individual per-server
//! consumers.
//!
//! The root process owns an [`h3x::dquic::Network`]; `register_listener`
//! calls [`Network::bind_server`] to obtain a [`ServerBinding`], then spawns a
//! drain task that forwards accepted connections to the registered per-server
//! mpsc channel. Each server gets a `PerServerListener` backed by that mpsc
//! receiver so the local/forwarded worker can drive [`h3x::quic::Listen`].
//!
//! [`Network::bind_server`]: h3x::dquic::Network::bind_server
//! [`ServerBinding`]: h3x::dquic::ServerBinding

use std::{
    fmt,
    sync::{Arc, Weak},
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::hypervisor::state::{RootState, ServiceOwner};

/// Error type for [`PerServerListener`].
///
/// Implements `std::error::Error + std::any::Any` as required by
/// [`h3x::quic::Listen::Error`].
#[derive(Debug)]
pub enum PerServerListenerError {
    /// The mpsc channel was closed (server removed or root shutting down).
    ChannelClosed,
    /// The adapter was explicitly shut down via [`PerServerListener::shutdown`].
    Shutdown,
}

impl fmt::Display for PerServerListenerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelClosed => write!(f, "per-server listen channel closed"),
            Self::Shutdown => write!(f, "per-server listener shut down"),
        }
    }
}

impl std::error::Error for PerServerListenerError {}

/// Per-server listener adapter.
///
/// Root creates one per `server_name`; the drain task attached to the
/// corresponding [`ServerBinding`] forwards accepted connections into this
/// adapter's mpsc channel. Wraps the receiver side so it implements
/// [`h3x::quic::Listen`].
///
/// [`ServerBinding`]: h3x::dquic::ServerBinding
pub struct PerServerListener {
    rx: mpsc::Receiver<Arc<h3x::dquic::prelude::Connection>>,
    shutdown_token: CancellationToken,
    root_state: Weak<RootState>,
    server_name: String,
    owner: ServiceOwner,
}

impl PerServerListener {
    /// Create a new per-server listen adapter.
    ///
    /// * `rx` — receives connections forwarded by the `ServerBinding` drain task
    /// * `shutdown_token` — signals shutdown of this adapter
    pub fn new_registered(
        rx: mpsc::Receiver<Arc<h3x::dquic::prelude::Connection>>,
        shutdown_token: CancellationToken,
        root_state: &Arc<RootState>,
        server_name: String,
        owner: ServiceOwner,
    ) -> Self {
        Self {
            rx,
            shutdown_token,
            root_state: Arc::downgrade(root_state),
            server_name,
            owner,
        }
    }
}

impl h3x::quic::Listen for PerServerListener {
    type Connection = h3x::dquic::prelude::Connection;
    type Error = PerServerListenerError;

    async fn accept(&mut self) -> Result<Arc<Self::Connection>, Self::Error> {
        tokio::select! {
            conn = self.rx.recv() => {
                conn.ok_or(PerServerListenerError::ChannelClosed)
            }
            _ = self.shutdown_token.cancelled() => {
                Err(PerServerListenerError::Shutdown)
            }
        }
    }

    async fn shutdown(&self) -> Result<(), Self::Error> {
        self.shutdown_token.cancel();
        if let Some(root_state) = self.root_state.upgrade() {
            root_state
                .release_server(&self.server_name, self.owner)
                .await;
        }
        Ok(())
    }
}

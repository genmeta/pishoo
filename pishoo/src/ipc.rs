//! Shared IPC protocol types for root ↔ worker communication over remoc.
//!
//! This module defines:
//! - [`ControlPlane`]: the remoc RTC trait exposing root's QUIC control
//!   plane to workers (returns [`RemoteListener`] / [`RemoteConnector`]).
//! - [`WorkerBootstrap`] / [`WorkerHello`]: one-shot bootstrap handshake.
//! - Error types for control plane operations.

use std::path::PathBuf;

use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::remoc::quic::{RemoteConnector, RemoteListener};
use serde::{Deserialize, Serialize};
use snafu::Snafu;

// ---------------------------------------------------------------------------
// Bootstrap handshake (one-shot, sent over remoc base channel)
// ---------------------------------------------------------------------------

/// Sent from root to worker immediately after establishing the remoc channel.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerBootstrap {
    pub uid: u32,
    pub username: String,
    pub home: PathBuf,
    /// RPC client for calling the root control plane.
    pub control_plane: ControlPlaneClient,
}

/// Sent from worker to root to confirm identity after receiving bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHello {
    pub pid: u32,
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    pub egid: u32,
}

// ---------------------------------------------------------------------------
// Control plane errors
// ---------------------------------------------------------------------------

/// Error returned by [`ControlPlane::listen`].
#[derive(Debug, Clone, Serialize, Deserialize, Snafu)]
#[snafu(module)]
pub enum ListenError {
    #[snafu(display("listener conflicts with an existing listener"))]
    Conflict,
    #[snafu(display("invalid listen request: {reason}"))]
    InvalidRequest { reason: String },
    #[snafu(display("internal error: {message}"))]
    Internal { message: String },
    #[snafu(transparent)]
    Call { source: remoc::rtc::CallError },
}

/// Error returned by [`ControlPlane::connect`].
#[derive(Debug, Clone, Serialize, Deserialize, Snafu)]
#[snafu(module)]
pub enum ConnectError {
    #[snafu(display("invalid connector profile `{profile}`"))]
    InvalidProfile { profile: String },
    #[snafu(display("internal error: {message}"))]
    Internal { message: String },
    #[snafu(transparent)]
    Call { source: remoc::rtc::CallError },
}

/// Error returned by [`ControlPlane::spawn_session`].
#[derive(Debug, Clone, Serialize, Deserialize, Snafu)]
#[snafu(module)]
pub enum SpawnSessionError {
    #[snafu(display("failed to spawn session process: {reason}"))]
    SpawnFailed { reason: String },
    #[snafu(display("session spawning is not supported"))]
    NotSupported,
    #[snafu(transparent)]
    Call { source: remoc::rtc::CallError },
}

// ---------------------------------------------------------------------------
// ControlPlane — remoc RTC trait
// ---------------------------------------------------------------------------

/// Remote trait exposing the root process's QUIC control plane to workers.
///
/// Workers call these methods to request listeners and connectors from root.
/// The returned [`RemoteListener`] / [`RemoteConnector`] directly implement
/// [`h3x::quic::Listen`] / [`h3x::quic::Connect`].
#[remoc::rtc::remote]
pub trait ControlPlane: Send + Sync {
    /// Request a QUIC listener for the given server configuration.
    ///
    /// Root creates the listener, starts serving connections over remoc,
    /// and returns a [`RemoteListener`] that the worker can use directly
    /// with h3x.
    async fn listener(&self, request: ListenRequest) -> Result<RemoteListener, ListenError>;

    /// Request an outbound QUIC connector.
    ///
    /// Root creates the connector, starts serving connect requests over
    /// remoc, and returns a [`RemoteConnector`] that the worker can use
    /// directly with h3x.
    async fn connector(&self, request: ConnectorRequest) -> Result<RemoteConnector, ConnectError>;

    /// Request root to spawn an SSH session child process for the given user.
    ///
    /// Root forks `pishoo-ssh-session` as root (for PAM), then sends the
    /// child's pipe FDs to the worker via the seqpacket side-channel
    /// (SCM_RIGHTS). This RPC returns `Ok(())` only after the FDs have
    /// been sent — the worker must read them from the seqpacket socket
    /// immediately after this call returns.
    async fn spawn_session(&self, username: String) -> Result<(), SpawnSessionError>;
}

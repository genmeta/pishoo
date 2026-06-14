//! Shared IPC protocol types for root ↔ worker communication over remoc.
//!
//! This module defines:
//! - [`ControlPlane`]: the remoc RTC trait exposing root's QUIC control
//!   plane to workers (returns [`IpcListenClient`] / [`IpcConnectClient`]).
//! - [`WorkerBootstrap`] / [`WorkerHello`]: one-shot bootstrap handshake.
//! - Error types for control plane operations.

use std::path::PathBuf;

use dhttp::h3x::ipc::quic::{IpcConnectClient, IpcListenClient};
use gateway::control_plane::{ConnectorRequest, ListenRequest};
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

/// Error returned by [`ControlPlane::rebuild_listener`].
///
/// Rebuild atomically replaces an owned listener so the server name is never
/// momentarily vacant during reload.
#[derive(Debug, Clone, Serialize, Deserialize, Snafu)]
#[snafu(module)]
pub enum RebuildListenError {
    #[snafu(display("listener is not owned by this worker"))]
    NotOwner,
    #[snafu(display("server name conflicts with an existing listener"))]
    Conflict,
    #[snafu(display("replacement listener failed after old listener was destroyed: {reason}"))]
    Replacement { reason: String },
    #[snafu(display("invalid rebuild request: {reason}"))]
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
/// The returned [`IpcListenClient`] / [`IpcConnectClient`] are used by the
/// worker to construct [`IpcListener`] / [`IpcConnector`] with the local
/// [`FdTransfer`](dhttp::h3x::ipc::transport::FdTransfer).
#[remoc::rtc::remote]
pub trait ControlPlane: Send + Sync {
    /// Request a QUIC listener for the given server configuration.
    ///
    /// Root creates the listener, wraps it in an IPC `ListenAdapter`, and
    /// returns an [`IpcListenClient`] that the worker constructs an
    /// [`IpcListener`](dhttp::h3x::ipc::capability::listener::IpcListener) from.
    async fn listener(&self, request: ListenRequest) -> Result<IpcListenClient, ListenError>;

    /// Atomically replace a previously acquired listener with one matching the
    /// new request. The previous listener is destroyed by root as part of the
    /// same critical section, so the server name is never observed vacant.
    async fn rebuild_listener(
        &self,
        request: ListenRequest,
    ) -> Result<IpcListenClient, RebuildListenError>;

    /// Request an outbound QUIC connector.
    ///
    /// Root creates the connector, wraps it in an IPC `ConnectAdapter`, and
    /// returns an [`IpcConnectClient`] that the worker constructs an
    /// [`IpcConnector`](dhttp::h3x::ipc::capability::connector::IpcConnector) from.
    async fn connector(&self, request: ConnectorRequest) -> Result<IpcConnectClient, ConnectError>;

    /// Request root to spawn an SSH session child process for the given user.
    ///
    /// Root forks `pishoo-ssh-session` as root (for PAM), then delivers the
    /// child's MuxChannel FD to the worker through the worker-chosen `fd_id`.
    /// The returned `u64` echoes `fd_id` after the FD delivery is queued to
    /// the local mux writer FIFO.
    async fn spawn_session(&self, username: String, fd_id: u64) -> Result<u64, SpawnSessionError>;
}

//! Shared IPC protocol types for root ↔ worker communication over remoc.

use std::path::PathBuf;

use h3x::remoc::quic::{RemoteQuicConnector, RemoteQuicListener};
use serde::{Deserialize, Serialize};

// --- Type aliases ---
pub type Pid = u32;
pub type Uid = u32;
pub type ServerName = String;

// --- Worker bootstrap (sent root → worker at startup) ---
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerBootstrap {
    pub uid: Uid,
    pub username: String,
    pub home: PathBuf,
    pub log_dir: PathBuf,
    /// Signal channel from root → worker (embedded in bootstrap, sent via remoc).
    pub signal_rx: remoc::rch::mpsc::Receiver<RootToWorker>,
    /// RPC client for calling root transport API from the worker.
    pub root_api: RootTransportApiClient,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHello {
    pub pid: Pid,
    pub uid: Uid,
    pub euid: Uid,
    pub gid: u32,
    pub egid: u32,
}

// --- Root → Worker messages ---
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RootToWorker {
    Signal(WorkerSignal),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerSignal {
    Reload,
    Quit,
    Terminate,
    ReopenLogs,
}

// --- Worker → Root request types ---
#[derive(Debug, Serialize, Deserialize)]
pub struct RequestListen {
    pub server_name: ServerName,
    pub bind: Vec<String>, // bind URIs as strings for now
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReleaseListen {
    pub server_name: ServerName,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenConnector {
    pub profile: String,
}

// --- Error types ---
#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum ListenRequestError {
    #[error("listener conflicts with an existing listener")]
    Conflict,
    #[error("invalid listen request: {0}")]
    InvalidRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("RPC call error: {0}")]
    Call(#[from] remoc::rtc::CallError),
}

#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum ReleaseListenError {
    #[error("caller does not own the listener")]
    NotOwner,
    #[error("listener not found")]
    NotFound,
    #[error("internal error: {0}")]
    Internal(String),
    #[error("RPC call error: {0}")]
    Call(#[from] remoc::rtc::CallError),
}

#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum OpenConnectorError {
    #[error("invalid connector profile")]
    InvalidProfile,
    #[error("internal error: {0}")]
    Internal(String),
    #[error("RPC call error: {0}")]
    Call(#[from] remoc::rtc::CallError),
}

// --- RTC trait for root transport API ---
// The `#[remoc::rtc::remote]` attribute generates `RootTransportApiClient` and
// `RootTransportApiServer*` types automatically.
#[remoc::rtc::remote]
pub trait RootTransportApi: Send + Sync {
    async fn request_listen(
        &self,
        request: RequestListen,
    ) -> Result<RemoteQuicListener, ListenRequestError>;

    async fn release_listen(
        &self,
        request: ReleaseListen,
    ) -> Result<(), ReleaseListenError>;

    async fn open_connector(
        &self,
        request: OpenConnector,
    ) -> Result<RemoteQuicConnector, OpenConnectorError>;
}

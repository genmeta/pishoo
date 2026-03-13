//! Shared IPC protocol types for root ↔ worker communication over remoc.

use std::path::PathBuf;

use h3x::remoc::quic::{ListenClient, RemoteConnectClient};
use serde::{Deserialize, Serialize};
use snafu::Snafu;

pub type ServerName = String;

// --- Worker bootstrap (sent root → worker at startup) ---
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerBootstrap {
    pub uid: u32,
    pub username: String,
    pub home: PathBuf,
    pub log_dir: PathBuf,
    /// RPC client for calling root transport API from the worker.
    pub root_api: RootTransportApiClient,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHello {
    pub pid: u32,
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    pub egid: u32,
}

// --- Worker → Root request types ---
#[derive(Debug, Serialize, Deserialize)]
pub struct RequestListen {
    pub server_name: ServerName,
    pub bind: Vec<String>,
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
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
#[derive(Debug, Serialize, Deserialize, Snafu)]
#[snafu(module)]
pub enum ListenRequestError {
    #[snafu(display("listener conflicts with an existing listener"))]
    Conflict,
    #[snafu(display("invalid listen request: {message}"))]
    InvalidRequest { message: String },
    #[snafu(display("internal error: {message}"))]
    Internal { message: String },
    #[snafu(display("RPC call error: {source}"))]
    Call { source: remoc::rtc::CallError },
}

impl From<remoc::rtc::CallError> for ListenRequestError {
    fn from(source: remoc::rtc::CallError) -> Self {
        Self::Call { source }
    }
}

#[derive(Debug, Serialize, Deserialize, Snafu)]
#[snafu(module)]
pub enum ReleaseListenError {
    #[snafu(display("caller does not own the listener"))]
    NotOwner,
    #[snafu(display("listener not found"))]
    NotFound,
    #[snafu(display("internal error: {message}"))]
    Internal { message: String },
    #[snafu(display("RPC call error: {source}"))]
    Call { source: remoc::rtc::CallError },
}

impl From<remoc::rtc::CallError> for ReleaseListenError {
    fn from(source: remoc::rtc::CallError) -> Self {
        Self::Call { source }
    }
}

#[derive(Debug, Serialize, Deserialize, Snafu)]
#[snafu(module)]
pub enum OpenConnectorError {
    #[snafu(display("invalid connector profile"))]
    InvalidProfile,
    #[snafu(display("internal error: {message}"))]
    Internal { message: String },
    #[snafu(display("RPC call error: {source}"))]
    Call { source: remoc::rtc::CallError },
}

impl From<remoc::rtc::CallError> for OpenConnectorError {
    fn from(source: remoc::rtc::CallError) -> Self {
        Self::Call { source }
    }
}

// --- RTC trait for root transport API ---
// The `#[remoc::rtc::remote]` attribute generates `RootTransportApiClient` and
// `RootTransportApiServer*` types automatically.
#[remoc::rtc::remote]
pub trait RootTransportApi: Send + Sync {
    async fn request_listen(
        &self,
        request: RequestListen,
    ) -> Result<ListenClient, ListenRequestError>;

    async fn release_listen(
        &self,
        request: ReleaseListen,
    ) -> Result<(), ReleaseListenError>;

    async fn open_connector(
        &self,
        request: OpenConnector,
    ) -> Result<RemoteConnectClient, OpenConnectorError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_listen_payload_is_material_not_paths() {
        let req = RequestListen {
            server_name: "example.test".to_string(),
            bind: vec![],
            cert_pem: b"-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----".to_vec(),
            key_pem: b"-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----".to_vec(),
        };
        assert_eq!(req.server_name, "example.test");
        assert!(!req.cert_pem.is_empty());
        assert!(!req.key_pem.is_empty());
    }
}

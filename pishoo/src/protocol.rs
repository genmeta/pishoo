//! Shared IPC protocol types for root ↔ worker communication over remoc.

use std::path::PathBuf;

use genmeta_home::identity::Name;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use snafu::Snafu;

use crate::remoc_bridge::{ConnectorHandle, ListenerHandle};

// --- Worker bootstrap (sent root → worker at startup) ---
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerBootstrap {
    pub uid: u32,
    pub username: String,
    pub home: PathBuf,
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
#[derive(Debug)]
pub struct RequestListen {
    pub name: Name<'static>,
    pub bind: Vec<String>,
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

mod request_listen_serde {
    use super::*;

    // TODO: avoid clones in (de)serialization
    #[derive(Debug, Serialize, Deserialize)]
    struct RequestListenSerdeHelper {
        name: Name<'static>,
        bind: Vec<String>,
        certs: Vec<Vec<u8>>,
        key: Vec<u8>,
    }

    impl From<&RequestListen> for RequestListenSerdeHelper {
        fn from(req: &RequestListen) -> Self {
            Self {
                name: req.name.clone(),
                bind: req.bind.clone(),
                certs: req.certs.iter().map(|cert| cert.to_vec()).collect(),
                key: req.key.secret_der().to_vec(),
            }
        }
    }

    impl TryFrom<RequestListenSerdeHelper> for RequestListen {
        type Error = &'static str;

        fn try_from(helper: RequestListenSerdeHelper) -> Result<Self, Self::Error> {
            Ok(Self {
                name: helper.name,
                bind: helper.bind,
                certs: helper
                    .certs
                    .into_iter()
                    .map(|cert| CertificateDer::from(cert))
                    .collect(),
                key: PrivateKeyDer::try_from(helper.key)?,
            })
        }
    }

    impl Serialize for RequestListen {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            RequestListenSerdeHelper::from(self).serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for RequestListen {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let helper = RequestListenSerdeHelper::deserialize(deserializer)?;
            Self::try_from(helper).map_err(serde::de::Error::custom)
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReleaseListen {
    pub server_name: Name<'static>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenConnector {
    pub profile: String,
}

// --- Error types ---
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ListenRequestInvalidReason {
    CertTooLarge { actual: usize, limit: usize },
    KeyTooLarge { actual: usize, limit: usize },
    InvalidCertificatePem,
    EmptyCertificate,
    InvalidPrivateKeyPem,
    EmptyPrivateKey,
}

impl std::fmt::Display for ListenRequestInvalidReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CertTooLarge { actual, limit } => {
                write!(f, "certificate pem too large ({actual} > {limit})")
            }
            Self::KeyTooLarge { actual, limit } => {
                write!(f, "private key pem too large ({actual} > {limit})")
            }
            Self::InvalidCertificatePem => write!(f, "invalid certificate pem"),
            Self::EmptyCertificate => write!(f, "certificate pem contains no certificates"),
            Self::InvalidPrivateKeyPem => write!(f, "invalid private key pem"),
            Self::EmptyPrivateKey => write!(f, "private key pem contains no key"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Snafu)]
#[snafu(module)]
pub enum ListenRequestError {
    #[snafu(display("listener conflicts with an existing listener"))]
    Conflict,
    #[snafu(display("invalid listen request: {reason}"))]
    InvalidRequest { reason: ListenRequestInvalidReason },
    #[snafu(display("internal error: {message}"))]
    Internal { message: String },
    #[snafu(display("rpc call error"))]
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
    #[snafu(display("rpc call error"))]
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
    #[snafu(display("rpc call error"))]
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
    ) -> Result<ListenerHandle, ListenRequestError>;

    async fn release_listen(&self, request: ReleaseListen) -> Result<(), ReleaseListenError>;

    async fn open_connector(
        &self,
        request: OpenConnector,
    ) -> Result<ConnectorHandle, OpenConnectorError>;
}

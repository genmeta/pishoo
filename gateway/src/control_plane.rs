use std::future::Future;
#[cfg(feature = "sshd")]
use std::os::fd::OwnedFd;

#[cfg(feature = "sshd")]
use futures::future::BoxFuture;
use genmeta_home::identity::Name;
pub use genmeta_home::identity::ssl::Identity;
use h3x::quic;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};

use crate::parse as gateway_parse;

/// A simple error type wrapping a string message.
///
/// Implements [`Display`](std::fmt::Display) to show its content and
/// [`Error`](std::error::Error) with no source. Designed for use as a
/// `source` in snafu error variants where the underlying error is a
/// dynamic message.
#[derive(Debug)]
pub struct StringError(pub String);

impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StringError {}

// ---------------------------------------------------------------------------
// Capability trait: ProvideListener
// ---------------------------------------------------------------------------

/// A request to create a QUIC listener for a specific server.
#[derive(Debug)]
pub struct ListenRequest {
    /// The identity to use for the server's TLS configuration.
    pub identity: Identity,
    /// Listen specifications; resolved to bind URIs by the root process.
    pub bind: Vec<gateway_parse::Listens>,
}

/// Capability to create QUIC listeners.
pub trait ProvideListener: Send + Sync {
    /// The listener type returned by [`listener()`](Self::listener).
    type Listener: quic::Listen;
    /// Error type for [`listener()`](Self::listener) operations.
    type ListenError: std::error::Error + Send + Sync;

    /// Request the control plane to create a QUIC listener for the given
    /// server configuration.
    fn listener(
        &self,
        request: ListenRequest,
    ) -> impl Future<Output = Result<Self::Listener, Self::ListenError>> + Send + '_;
}

// ---------------------------------------------------------------------------
// Capability trait: ProvideConnector
// ---------------------------------------------------------------------------

/// A request to create an outbound QUIC connector.
///
/// The optional [`identity`](Self::identity) allows workers to authenticate
/// with their own mTLS certificate when connecting to other servers,
/// independent of the root process's identity.
#[derive(Debug)]
pub struct ConnectorRequest {
    /// Optional TLS identity for outbound mTLS authentication.
    /// When `None`, the connector will not present a client certificate.
    pub identity: Option<Identity>,
}

/// Capability to create outbound QUIC connectors.
pub trait ProvideConnector: Send + Sync {
    /// The connector type returned by [`connector()`](Self::connector).
    type Connector: quic::Connect;
    /// Error type for [`connector()`](Self::connector) operations.
    type ConnectError: std::error::Error + Send + Sync;

    /// Request the control plane to create an outbound QUIC connector.
    fn connector(
        &self,
        request: ConnectorRequest,
    ) -> impl Future<Output = Result<Self::Connector, Self::ConnectError>> + Send + '_;
}

// ---------------------------------------------------------------------------
// Capability trait: SpawnSession (sshd feature)
// ---------------------------------------------------------------------------

/// Transport handles for communicating with a spawned SSH session process.
///
/// Contains the raw pipe file descriptors of the child process. The
/// consumer is responsible for converting these into the async IO type
/// it needs (e.g. `tokio::fs::File`).
#[cfg(feature = "sshd")]
pub struct SessionTransport {
    /// Write end of the child's stdin pipe.
    pub stdin: OwnedFd,
    /// Read end of the child's stdout pipe.
    pub stdout: OwnedFd,
}

/// Concrete (AFIT) trait for spawning SSH session child processes.
///
/// Implement this for each control plane variant. A blanket impl
/// provides [`DynSpawnSession`] automatically.
#[cfg(feature = "sshd")]
pub trait SpawnSession: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn spawn_session(
        &self,
        username: &str,
    ) -> impl Future<Output = Result<SessionTransport, Self::Error>> + Send;
}

/// Object-safe version of [`SpawnSession`] with type-erased error.
#[cfg(feature = "sshd")]
pub trait DynSpawnSession: Send + Sync {
    fn spawn_session<'a>(
        &'a self,
        username: &'a str,
    ) -> BoxFuture<'a, Result<SessionTransport, Box<dyn std::error::Error + Send + Sync>>>;
}

#[cfg(feature = "sshd")]
impl<T: SpawnSession> DynSpawnSession for T {
    fn spawn_session<'a>(
        &'a self,
        username: &'a str,
    ) -> BoxFuture<'a, Result<SessionTransport, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async move {
            SpawnSession::spawn_session(self, username)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        })
    }
}

// ---------------------------------------------------------------------------
// ControlPlane: aggregation of capabilities
// ---------------------------------------------------------------------------

/// Aggregated marker trait combining all control plane capabilities.
///
/// Two implementations exist:
///
/// - **`RemoteControlPlane`**: used by worker processes, communicates with
///   root via remoc RPC.
/// - **`LocalControlPlane`**: used by root-local services, directly
///   accessing the root state in-process.
///
/// When the `sshd` feature is enabled, implementors must also provide
/// [`SpawnSession`] so that SSH3 handlers can spawn session child
/// processes through the control plane.
#[cfg(feature = "sshd")]
pub trait ControlPlane: ProvideListener + ProvideConnector + SpawnSession {}

#[cfg(feature = "sshd")]
impl<T: ProvideListener + ProvideConnector + SpawnSession> ControlPlane for T {}

#[cfg(not(feature = "sshd"))]
pub trait ControlPlane: ProvideListener + ProvideConnector {}

#[cfg(not(feature = "sshd"))]
impl<T: ProvideListener + ProvideConnector> ControlPlane for T {}

// ---------------------------------------------------------------------------
// Custom serde for ListenRequest / ConnectorRequest
// (Identity / CertificateDer / PrivateKeyDer are not natively serializable)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct IdentityHelper {
    name: Name<'static>,
    certs: Vec<Vec<u8>>,
    key: Vec<u8>,
}

impl IdentityHelper {
    fn from_identity(id: &Identity) -> Self {
        Self {
            name: id.name().clone(),
            certs: id.certs().iter().map(|c| c.to_vec()).collect(),
            key: id.key().secret_der().to_vec(),
        }
    }

    fn into_identity<E: serde::de::Error>(self) -> Result<Identity, E> {
        let certs = self.certs.into_iter().map(CertificateDer::from).collect();
        let key = PrivateKeyDer::try_from(self.key).map_err(E::custom)?;
        Ok(Identity::new(self.name, certs, key))
    }
}

#[derive(Serialize, Deserialize)]
struct ListenRequestHelper {
    identity: IdentityHelper,
    bind: Vec<gateway_parse::Listens>,
}

impl Serialize for ListenRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ListenRequestHelper {
            identity: IdentityHelper::from_identity(&self.identity),
            bind: self.bind.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ListenRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let helper = ListenRequestHelper::deserialize(deserializer)?;
        Ok(Self {
            identity: helper.identity.into_identity()?,
            bind: helper.bind,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ConnectorRequestHelper {
    identity: Option<IdentityHelper>,
}

impl Serialize for ConnectorRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ConnectorRequestHelper {
            identity: self.identity.as_ref().map(IdentityHelper::from_identity),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConnectorRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let helper = ConnectorRequestHelper::deserialize(deserializer)?;
        Ok(Self {
            identity: helper
                .identity
                .map(IdentityHelper::into_identity)
                .transpose()?,
        })
    }
}

use std::future::Future;

use genmeta_home::identity::Name;
pub use genmeta_home::identity::ssl::Identity;
use h3x::quic;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};

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

/// A request to create a QUIC listener for a specific server.
#[derive(Debug)]
pub struct ListenRequest {
    /// The identity to use for the server's TLS configuration.
    pub identity: Identity,
    /// Bind addresses (e.g., `["0.0.0.0:443", "[::]:443"]`).
    pub bind: Vec<String>,
}

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

/// Abstraction over the control plane that provides QUIC listener and
/// connector creation capabilities.
///
/// This trait is the boundary between the service layer (HTTP/3 request
/// handling) and the infrastructure layer (QUIC networking). Two
/// implementations exist:
///
/// - **`RemoteControlPlane`**: used by worker processes, communicates with
///   root via remoc RPC. Returns [`h3x::remoc::quic::RemoteListener`] /
///   [`h3x::remoc::quic::RemoteConnector`].
/// - **`LocalControlPlane`**: used by root-local services, directly
///   accessing the root state in-process.
pub trait ControlPlane: Send + Sync {
    /// The listener type returned by [`listener()`](Self::listener).
    type Listener: quic::Listen;

    /// The connector type returned by [`connector()`](Self::connector).
    type Connector: quic::Connect;

    /// Error type for [`listener()`](Self::listener) operations.
    type ListenError: std::error::Error + Send + Sync;

    /// Error type for [`connector()`](Self::connector) operations.
    type ConnectError: std::error::Error + Send + Sync;

    /// Request the control plane to create a QUIC listener for the given
    /// server configuration. The returned listener can be used with
    /// [`h3x`] to serve HTTP/3 connections.
    fn listener(
        &self,
        request: ListenRequest,
    ) -> impl Future<Output = Result<Self::Listener, Self::ListenError>> + Send + '_;

    /// Request the control plane to create an outbound QUIC connector.
    /// The returned connector can be used by the forward proxy to establish
    /// outbound QUIC connections.
    fn connector(
        &self,
        request: ConnectorRequest,
    ) -> impl Future<Output = Result<Self::Connector, Self::ConnectError>> + Send + '_;
}

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
    bind: Vec<String>,
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

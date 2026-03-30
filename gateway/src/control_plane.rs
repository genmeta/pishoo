use std::future::Future;

use genmeta_home::identity::Name;
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

/// TLS identity for a genmeta service (name + certificates + private key).
#[derive(Debug)]
pub struct Identity {
    /// The genmeta identity name.
    pub name: Name<'static>,
    /// TLS certificate chain in DER format.
    pub certs: Vec<CertificateDer<'static>>,
    /// TLS private key in DER format.
    pub key: PrivateKeyDer<'static>,
}

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
// Custom serde for Identity / ListenRequest / ConnectorRequest
// (CertificateDer / PrivateKeyDer are not natively serializable)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct IdentityHelper {
    name: Name<'static>,
    certs: Vec<Vec<u8>>,
    key: Vec<u8>,
}

impl Serialize for Identity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        IdentityHelper {
            name: self.name.clone(),
            certs: self.certs.iter().map(|c| c.to_vec()).collect(),
            key: self.key.secret_der().to_vec(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let helper = IdentityHelper::deserialize(deserializer)?;
        Ok(Self {
            name: helper.name,
            certs: helper.certs.into_iter().map(CertificateDer::from).collect(),
            key: PrivateKeyDer::try_from(helper.key).map_err(serde::de::Error::custom)?,
        })
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
            identity: IdentityHelper {
                name: self.identity.name.clone(),
                certs: self.identity.certs.iter().map(|c| c.to_vec()).collect(),
                key: self.identity.key.secret_der().to_vec(),
            },
            bind: self.bind.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ListenRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let helper = ListenRequestHelper::deserialize(deserializer)?;
        Ok(Self {
            identity: Identity {
                name: helper.identity.name,
                certs: helper
                    .identity
                    .certs
                    .into_iter()
                    .map(CertificateDer::from)
                    .collect(),
                key: PrivateKeyDer::try_from(helper.identity.key)
                    .map_err(serde::de::Error::custom)?,
            },
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
            identity: self.identity.as_ref().map(|id| IdentityHelper {
                name: id.name.clone(),
                certs: id.certs.iter().map(|c| c.to_vec()).collect(),
                key: id.key.secret_der().to_vec(),
            }),
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
                .map(|h| {
                    Ok::<_, D::Error>(Identity {
                        name: h.name,
                        certs: h.certs.into_iter().map(CertificateDer::from).collect(),
                        key: PrivateKeyDer::try_from(h.key).map_err(serde::de::Error::custom)?,
                    })
                })
                .transpose()?,
        })
    }
}

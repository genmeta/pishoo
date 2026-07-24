use std::sync::Arc;

use dhttp::name::DhttpName;
use gateway::{
    control_plane::ListenRequest,
    parse::{
        config::{ServerConfig, ServerIdentity},
        types::Listens,
    },
    reverse::router::RouterState,
};
use snafu::{ResultExt, Snafu};

use super::{resource::AccessLogResourcePlan, snapshot::PreparedServerService};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenRequestFingerprint {
    pub server_name: DhttpName<'static>,
    pub bind_debug: String,
    pub identity_debug: String,
    pub dns_resolver_debug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerSpec {
    pub request_fingerprint: ListenRequestFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFingerprint {
    pub listener_spec: ListenerSpec,
    pub service_generation: u64,
}

pub struct PreparedServerUpdate {
    pub name: DhttpName<'static>,
    pub listen_request: ListenRequest,
    pub listener_spec: ListenerSpec,
    pub service: PreparedServerService,
    pub access_logs: AccessLogResourcePlan,
    pub fingerprint: ServerFingerprint,
}

#[derive(Debug, Snafu)]
#[snafu(module(prepare_server_update_error))]
pub enum PrepareServerUpdateError {
    #[snafu(display("failed to load access policy for server `{name}`"))]
    Policy {
        name: String,
        source: crate::policy::PolicyError,
    },
    #[snafu(display("failed to materialize access log configuration for server `{name}`"))]
    AccessLog {
        name: String,
        source: gateway::parse::config::MaterializeAccessLogError,
    },
    #[cfg(test)]
    #[snafu(display("synthetic prepare failure for {server_name}"))]
    SyntheticFailure { server_name: String },
}

pub enum ServerSource {
    Typed(TypedServerSource),
    #[cfg(test)]
    Fake(FakeServerSource),
}

pub struct TypedServerSource {
    name: DhttpName<'static>,
    identity: dhttp::identity::Identity,
    bind: Vec<Listens>,
    dns_resolver_url: Option<http::Uri>,
    server_config: Arc<ServerConfig>,
}

pub struct PrepareContext {
    pub h3_settings: Arc<dhttp::h3x::dhttp::settings::Settings>,
    pub router_state: RouterState,
}

impl PrepareContext {
    pub fn new(router_state: RouterState) -> Self {
        let settings = dhttp::h3x::dhttp::settings::Settings::default()
            .with(dhttp::h3x::extended_connect::settings::EnableConnectProtocol::setting(true));
        #[cfg(feature = "sshd")]
        let settings = settings
            .with_all(dhttp::h3x::dhttp::webtransport::settings::WebTransportSupport::default());
        Self {
            h3_settings: Arc::new(settings),
            router_state,
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(module(build_typed_server_source_error))]
pub enum BuildTypedServerSourceError {
    #[snafu(display("server has no listen directive"))]
    MissingListen,
    #[snafu(display("server has no server_name"))]
    MissingName,
    #[snafu(display("failed to read certificate at `{}`", path.display()))]
    ReadCert {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to read private key at `{}`", path.display()))]
    ReadKey {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("invalid TLS material"))]
    InvalidTls {
        source: crate::tls::TlsMaterialError,
    },
    #[snafu(display("failed to load identity `{name}`"))]
    LoadIdentity {
        name: String,
        source: dhttp::home::identity::ssl::LoadIdentityError,
    },
}

impl TypedServerSource {
    pub async fn load_all(
        configs: impl IntoIterator<Item = Arc<ServerConfig>>,
        router_state: RouterState,
    ) -> (Vec<ServerSource>, PrepareContext) {
        let mut sources = Vec::new();
        let mut names = std::collections::HashSet::new();
        let loads = configs.into_iter().map(Self::load_config);
        for result in futures::future::join_all(loads).await {
            match result {
                Ok(config_sources) => {
                    for source in config_sources {
                        if names.insert(source.name.clone()) {
                            sources.push(ServerSource::Typed(source));
                        } else {
                            tracing::warn!(server_name = %source.name, "duplicate server name stopped");
                        }
                    }
                }
                Err(error) => tracing::warn!(
                    error = %snafu::Report::from_error(&error),
                    "server resource construction failed"
                ),
            }
        }
        (sources, PrepareContext::new(router_state))
    }

    async fn load_config(
        config: Arc<ServerConfig>,
    ) -> Result<Vec<Self>, BuildTypedServerSourceError> {
        let bind = config
            .listens()
            .iter()
            .flat_map(|listen| listen.0.clone())
            .collect::<Vec<_>>();
        if bind.is_empty() {
            return Err(BuildTypedServerSourceError::MissingListen);
        }
        if config.names().is_empty() {
            return Err(BuildTypedServerSourceError::MissingName);
        }
        let identity = load_identity(&config).await?;
        let resolver = config.resolver().map(|resolver| resolver.0.clone());
        Ok(config
            .names()
            .iter()
            .map(|name| Self {
                name: name.clone(),
                identity: identity.clone(),
                bind: bind.clone(),
                dns_resolver_url: resolver.clone(),
                server_config: config.clone(),
            })
            .collect())
    }

    async fn prepare(
        &self,
        context: &PrepareContext,
    ) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        let access_rules_uri = self
            .server_config
            .http()
            .access_rules()
            .effective()
            .as_ref()
            .map(|uri| uri.0.as_str());
        let identity_profile = match self.server_config.identity() {
            ServerIdentity::Profile(profile) => Some(profile),
            ServerIdentity::Direct { .. } => None,
        };
        let policy = crate::policy::load_policy_bundle(access_rules_uri, identity_profile)
            .await
            .context(prepare_server_update_error::PolicySnafu {
                name: self.name.to_string(),
            })?;
        let listen_request = ListenRequest {
            identity: self.identity.clone(),
            bind: self.bind.clone(),
            dns_resolver_url: self.dns_resolver_url.clone(),
        };
        let listener_spec = ListenerSpec {
            request_fingerprint: ListenRequestFingerprint {
                server_name: self.name.clone(),
                bind_debug: format!("{:?}", self.bind),
                identity_debug: compute_identity_fingerprint(&self.identity),
                dns_resolver_debug: self.dns_resolver_url.as_ref().map(ToString::to_string),
            },
        };
        let access_logs = AccessLogResourcePlan {
            server: self
                .server_config
                .http()
                .access_log()
                .effective()
                .materialize(self.server_config.identity())
                .context(prepare_server_update_error::AccessLogSnafu {
                    name: self.name.to_string(),
                })?,
            locations: self
                .server_config
                .locations()
                .iter()
                .map(|location| {
                    location
                        .http()
                        .access_log()
                        .effective()
                        .materialize(self.server_config.identity())
                        .context(prepare_server_update_error::AccessLogSnafu {
                            name: self.name.to_string(),
                        })
                })
                .collect::<Result<Box<[_]>, _>>()?,
        };
        let service = PreparedServerService {
            h3_settings: context.h3_settings.clone(),
            access_rules: policy.location_rules,
            router_state: context.router_state.clone(),
            server_config: self.server_config.clone(),
            server_name: self.name.clone(),
        };
        let service_generation = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Ok(PreparedServerUpdate {
            name: self.name.clone(),
            listen_request,
            listener_spec: listener_spec.clone(),
            service,
            access_logs,
            fingerprint: ServerFingerprint {
                listener_spec,
                service_generation,
            },
        })
    }
}

async fn load_identity(
    config: &ServerConfig,
) -> Result<dhttp::identity::Identity, BuildTypedServerSourceError> {
    match config.identity() {
        ServerIdentity::Profile(profile) => profile.load_identity().await.context(
            build_typed_server_source_error::LoadIdentitySnafu {
                name: profile.name().to_string(),
            },
        ),
        ServerIdentity::Direct {
            certificate,
            private_key,
        } => {
            let name = config
                .names()
                .first()
                .ok_or(BuildTypedServerSourceError::MissingName)?;
            let cert = tokio::fs::read(certificate.as_ref()).await.context(
                build_typed_server_source_error::ReadCertSnafu {
                    path: certificate.as_ref(),
                },
            )?;
            let key = tokio::fs::read(private_key.as_ref()).await.context(
                build_typed_server_source_error::ReadKeySnafu {
                    path: private_key.as_ref(),
                },
            )?;
            let (certs, key) = crate::tls::validate_tls_material(&cert, &key)
                .context(build_typed_server_source_error::InvalidTlsSnafu)?;
            Ok(dhttp::identity::Identity::new(
                name.clone().into(),
                certs,
                key,
            ))
        }
    }
}

impl ServerSource {
    pub fn name(&self) -> &DhttpName<'static> {
        match self {
            Self::Typed(source) => &source.name,
            #[cfg(test)]
            Self::Fake(source) => &source.name,
        }
    }
    pub async fn prepare(
        &self,
        context: &PrepareContext,
    ) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        match self {
            Self::Typed(source) => source.prepare(context).await,
            #[cfg(test)]
            Self::Fake(source) => source.prepare(),
        }
    }
}

pub(crate) fn compute_identity_fingerprint(identity: &dhttp::identity::Identity) -> String {
    use sha2::{Digest, Sha256};
    fn hex(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
    let mut certs = Sha256::new();
    for cert in identity.certs.iter() {
        certs.update(cert.as_ref());
    }
    let mut key = Sha256::new();
    key.update(identity.key.secret_der());
    format!(
        "{}@{}@{}",
        identity.name(),
        hex(certs.finalize()),
        hex(key.finalize())
    )
}

#[cfg(test)]
pub struct FakeServerSource {
    pub(crate) name: DhttpName<'static>,
    outcome: FakePrepareOutcome,
}
#[cfg(test)]
enum FakePrepareOutcome {
    Success {
        listener_spec: ListenerSpec,
        service_generation: u64,
    },
    Failure,
}
#[cfg(test)]
impl FakeServerSource {
    fn prepare(&self) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        match &self.outcome {
            FakePrepareOutcome::Success {
                listener_spec,
                service_generation,
            } => Ok(PreparedServerUpdate {
                name: self.name.clone(),
                listen_request: fake_listen_request(&self.name),
                listener_spec: listener_spec.clone(),
                service: super::snapshot::ServerService::fake(),
                access_logs: AccessLogResourcePlan {
                    server: gateway::parse::config::ResolvedAccessLogConfig::Disabled,
                    locations: Box::new([]),
                },
                fingerprint: ServerFingerprint {
                    listener_spec: listener_spec.clone(),
                    service_generation: *service_generation,
                },
            }),
            FakePrepareOutcome::Failure => Err(PrepareServerUpdateError::SyntheticFailure {
                server_name: self.name.to_string(),
            }),
        }
    }
}

#[cfg(test)]
impl ServerSource {
    pub(crate) fn fake_success(name: &str, generation: u64, listener_spec: ListenerSpec) -> Self {
        Self::Fake(FakeServerSource {
            name: fake_name(name),
            outcome: FakePrepareOutcome::Success {
                listener_spec,
                service_generation: generation,
            },
        })
    }
    pub(crate) fn fake_prepare_error(name: &str) -> Self {
        Self::Fake(FakeServerSource {
            name: fake_name(name),
            outcome: FakePrepareOutcome::Failure,
        })
    }
}

#[cfg(test)]
impl ListenerSpec {
    pub(crate) fn fake(label: &str) -> Self {
        Self {
            request_fingerprint: ListenRequestFingerprint {
                server_name: fake_name("fixture.dhttp.net"),
                bind_debug: format!("bind:{label}"),
                identity_debug: format!("identity:{label}"),
                dns_resolver_debug: None,
            },
        }
    }
}

#[cfg(test)]
fn fake_name(name: &str) -> DhttpName<'static> {
    DhttpName::try_from(name.to_owned()).unwrap()
}

#[cfg(test)]
fn fake_listen_request(name: &DhttpName<'static>) -> ListenRequest {
    let fqdn = name.as_full().to_owned();
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![fqdn.clone()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, &fqdn);
    let cert = params.self_signed(&key_pair).unwrap();
    ListenRequest {
        identity: dhttp::identity::Identity::new(
            name.clone().into(),
            vec![rustls::pki_types::CertificateDer::from(cert.der().to_vec())],
            rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap(),
        ),
        bind: vec![],
        dns_resolver_url: None,
    }
}

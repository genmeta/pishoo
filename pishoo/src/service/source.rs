use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

use dhttp::{home::identity::ssl::SSL_DIR_NAME, name::DhttpName};
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

const SSL_STAGE_DIR_PREFIX: &str = ".ssl-stage-";
const SSL_BACKUP_DIR_PREFIX: &str = ".ssl-backup-";

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
    #[snafu(display("failed to load server identity"))]
    Identity { source: BuildTypedServerSourceError },
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

#[derive(Clone)]
pub struct TypedServerSource {
    name: DhttpName<'static>,
    identity_source: TlsIdentitySource,
    bind: Vec<Listens>,
    dns_resolver_url: Option<http::Uri>,
    server_config: Arc<ServerConfig>,
}

#[derive(Clone, Debug)]
pub(crate) enum TlsIdentitySource {
    Profile(dhttp::home::identity::IdentityProfile),
    Direct {
        name: DhttpName<'static>,
        certificate: std::path::PathBuf,
        private_key: std::path::PathBuf,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum TlsIdentityWatchFilter {
    Profile { roots: Vec<PathBuf> },
    Direct { material_paths: Vec<PathBuf> },
}

impl TlsIdentityWatchFilter {
    pub(crate) fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Profile { roots } => roots.iter().any(|root| {
                let Ok(relative) = path.strip_prefix(root) else {
                    return false;
                };
                let Some(std::path::Component::Normal(component)) = relative.components().next()
                else {
                    return false;
                };

                component == OsStr::new(SSL_DIR_NAME)
                    || component.to_str().is_some_and(|component| {
                        component.starts_with(SSL_STAGE_DIR_PREFIX)
                            || component.starts_with(SSL_BACKUP_DIR_PREFIX)
                    })
            }),
            Self::Direct { material_paths } => material_paths.iter().any(|target| path == target),
        }
    }
}

impl TlsIdentitySource {
    pub(crate) async fn load(
        &self,
    ) -> Result<dhttp::identity::Identity, BuildTypedServerSourceError> {
        match self {
            Self::Profile(profile) => profile.load_identity().await.context(
                build_typed_server_source_error::LoadIdentitySnafu {
                    name: profile.name().to_string(),
                },
            ),
            Self::Direct {
                name,
                certificate,
                private_key,
            } => {
                let cert = tokio::fs::read(certificate.as_path()).await.context(
                    build_typed_server_source_error::ReadCertSnafu { path: certificate },
                )?;
                let key = tokio::fs::read(private_key.as_path())
                    .await
                    .context(build_typed_server_source_error::ReadKeySnafu { path: private_key })?;
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

    pub(crate) fn watch_targets(&self) -> Vec<(std::path::PathBuf, bool)> {
        match self {
            Self::Profile(profile) => vec![(profile.path().to_owned(), true)],
            Self::Direct {
                certificate,
                private_key,
                ..
            } => {
                let mut roots = Vec::new();
                for path in [certificate, private_key] {
                    let root = path.parent().unwrap_or(path.as_path()).to_owned();
                    if !roots.contains(&root) {
                        roots.push(root);
                    }
                }
                roots.into_iter().map(|root| (root, false)).collect()
            }
        }
    }

    pub(crate) fn watch_filter(&self) -> TlsIdentityWatchFilter {
        match self {
            Self::Profile(profile) => {
                let mut roots = vec![profile.path().to_owned()];
                if let Ok(canonical) = std::fs::canonicalize(profile.path())
                    && !roots.contains(&canonical)
                {
                    roots.push(canonical);
                }
                TlsIdentityWatchFilter::Profile { roots }
            }
            Self::Direct {
                certificate,
                private_key,
                ..
            } => {
                let mut material_paths = Vec::new();
                for path in [certificate, private_key] {
                    if !material_paths.contains(path) {
                        material_paths.push(path.clone());
                    }
                    if let Some(file_name) = path.file_name() {
                        let parent = path.parent().filter(|path| !path.as_os_str().is_empty());
                        if let Ok(canonical_parent) =
                            std::fs::canonicalize(parent.unwrap_or(Path::new(".")))
                        {
                            let canonical = canonical_parent.join(file_name);
                            if !material_paths.contains(&canonical) {
                                material_paths.push(canonical);
                            }
                        }
                    }
                }
                TlsIdentityWatchFilter::Direct { material_paths }
            }
        }
    }
}

#[derive(Clone)]
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
    pub(crate) fn name(&self) -> &DhttpName<'static> {
        &self.name
    }

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
        let identity_source = match config.identity() {
            ServerIdentity::Profile(profile) => TlsIdentitySource::Profile(profile.clone()),
            ServerIdentity::Direct {
                certificate,
                private_key,
            } => TlsIdentitySource::Direct {
                name: config
                    .names()
                    .first()
                    .ok_or(BuildTypedServerSourceError::MissingName)?
                    .clone(),
                certificate: certificate.as_ref().to_owned(),
                private_key: private_key.as_ref().to_owned(),
            },
        };
        let resolver = config.resolver().map(|resolver| resolver.0.clone());
        Ok(config
            .names()
            .iter()
            .map(|name| Self {
                name: name.clone(),
                identity_source: identity_source.clone(),
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
        let identity = self
            .identity_source
            .load()
            .await
            .context(prepare_server_update_error::IdentitySnafu)?;
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
            identity: identity.clone(),
            bind: self.bind.clone(),
            dns_resolver_url: self.dns_resolver_url.clone(),
        };
        let listener_spec = ListenerSpec {
            request_fingerprint: ListenRequestFingerprint {
                server_name: self.name.clone(),
                bind_debug: format!("{:?}", self.bind),
                identity_debug: compute_identity_fingerprint(&identity),
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

    pub(crate) fn identity_source(&self) -> &TlsIdentitySource {
        &self.identity_source
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

    pub(crate) fn typed(&self) -> Option<&TypedServerSource> {
        match self {
            Self::Typed(source) => Some(source),
            #[cfg(test)]
            Self::Fake(_) => None,
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

#[cfg(test)]
mod identity_source_tests {
    use super::*;

    #[test]
    fn profile_watches_the_whole_profile_for_atomic_ssl_directory_swaps() {
        let path = std::path::PathBuf::from("/tmp/watch.example.dhttp.net");
        let profile = dhttp::home::identity::IdentityProfile::try_from(path.clone()).unwrap();
        let source = TlsIdentitySource::Profile(profile);

        assert_eq!(source.watch_targets(), vec![(path, true)]);
    }

    #[test]
    fn direct_identity_watches_distinct_parent_directories_non_recursively() {
        let source = TlsIdentitySource::Direct {
            name: DhttpName::try_from("watch.example.dhttp.net".to_owned()).unwrap(),
            certificate: "/tmp/certs/fullchain.crt".into(),
            private_key: "/tmp/keys/privkey.pem".into(),
        };

        assert_eq!(
            source.watch_targets(),
            vec![("/tmp/certs".into(), false), ("/tmp/keys".into(), false)]
        );
    }

    #[test]
    fn profile_matches_only_tls_material_and_atomic_swap_paths() {
        let path = std::path::PathBuf::from("/tmp/watch.example.dhttp.net");
        let profile = dhttp::home::identity::IdentityProfile::try_from(path.clone()).unwrap();
        let source = TlsIdentitySource::Profile(profile);

        for changed in [
            path.join("ssl"),
            path.join("ssl/fullchain.crt"),
            path.join("ssl/privkey.pem"),
            path.join(".ssl-stage-123-1/fullchain.crt"),
            path.join(".ssl-backup-123-1/privkey.pem"),
        ] {
            assert!(
                source.watch_filter().matches(&changed),
                "{}",
                changed.display()
            );
        }

        for unchanged in [
            path.clone(),
            path.join("logs/access.log"),
            path.join("logs/cert.log"),
            path.join("db/access.db"),
            path.join("server.conf"),
            path.join("other/ssl/fullchain.crt"),
        ] {
            assert!(
                !source.watch_filter().matches(&unchanged),
                "{}",
                unchanged.display()
            );
        }
    }

    #[test]
    fn direct_identity_matches_only_configured_material_files() {
        let source = TlsIdentitySource::Direct {
            name: DhttpName::try_from("watch.example.dhttp.net".to_owned()).unwrap(),
            certificate: "/tmp/certs/fullchain.crt".into(),
            private_key: "/tmp/keys/privkey.pem".into(),
        };

        let filter = source.watch_filter();
        assert!(filter.matches(Path::new("/tmp/certs/fullchain.crt")));
        assert!(filter.matches(Path::new("/tmp/keys/privkey.pem")));
        assert!(!filter.matches(Path::new("/tmp/certs/access.log")));
        assert!(!filter.matches(Path::new("/tmp/keys/other.pem")));
    }
}

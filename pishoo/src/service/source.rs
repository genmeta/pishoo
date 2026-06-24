use std::sync::Arc;

use dhttp::{
    home::{DhttpHome, identity::IdentityProfile},
    name::DhttpName,
};
use gateway::{
    control_plane::ListenRequest,
    parse::{
        document::ConfigNode,
        types::{AccessRulesUri, ListenConfig, Listens, ResolverConfig, ServerNames},
    },
    reverse::router::RouterState,
};
use snafu::{OptionExt, Report, ResultExt, Snafu};

use super::snapshot::ServerService;
use crate::config::load_identity_servers;

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
    pub service: Arc<ServerService>,
    pub fingerprint: ServerFingerprint,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum PrepareServerUpdateError {
    #[snafu(display("failed to build identity service config"))]
    IdentityService {
        source: crate::worker::config::BuildConfigError,
    },
    #[cfg(test)]
    #[snafu(display("synthetic prepare failure for {server_name}"))]
    SyntheticFailure { server_name: String },
}

pub enum ServerSource {
    IdentityService(IdentityServiceSource),
    PishooConfig(PishooConfigServiceSource),
    #[cfg(test)]
    Fake(FakeServerSource),
}

pub struct PrepareContext {
    pub h3_settings: Arc<dhttp::h3x::dhttp::settings::Settings>,
    pub access_rules: Arc<dhttp::access::matcher::LocationRulesMatcher>,
    pub router_state: gateway::reverse::router::RouterState,
}

fn h3_settings() -> Arc<dhttp::h3x::dhttp::settings::Settings> {
    let settings = dhttp::h3x::dhttp::settings::Settings::default();
    #[cfg(feature = "sshd")]
    let settings = settings
        .with_all(dhttp::h3x::dhttp::webtransport::settings::WebTransportSupport::default());
    Arc::new(settings)
}

pub struct IdentityServiceSource {
    pub name: DhttpName<'static>,
    pub home: DhttpHome,
    pub identity_profile: IdentityProfile,
}

pub struct PishooConfigServiceSource {
    pub name: DhttpName<'static>,
    pub identity: dhttp::identity::Identity,
    pub bind: Vec<Listens>,
    pub dns_resolver_url: Option<http::Uri>,
    pub server_node: Arc<ConfigNode>,
}

#[cfg(test)]
pub struct FakeServerSource {
    pub(crate) name: DhttpName<'static>,
    pub(crate) outcome: FakePrepareOutcome,
}

#[cfg(test)]
pub enum FakePrepareOutcome {
    Success {
        listener_spec: ListenerSpec,
        service_generation: u64,
    },
    Failure,
}

impl PishooConfigServiceSource {
    pub async fn load_all(
        config_servers: &[Arc<ConfigNode>],
        home: Option<&DhttpHome>,
        router_state: RouterState,
    ) -> Result<(Vec<ServerSource>, PrepareContext), BuildConfigServiceSourcesError> {
        let canonicalized = crate::naming::canonicalize_server_nodes(config_servers)
            .context(build_config_service_sources_error::CanonicalizeSnafu)?;

        let mut access_rules_uri: Option<String> = None;
        let mut sources = Vec::new();
        let mut seen_server_names = std::collections::HashSet::new();

        for server in &canonicalized {
            let listens = listen_values(server)
                .context(build_config_service_sources_error::ConfigQuerySnafu)?;
            if listens.is_empty() {
                return build_config_service_sources_error::MissingListenSnafu.fail();
            }

            let server_names = server
                .get::<gateway::parse::types::ServerNames>("server_name")
                .context(build_config_service_sources_error::ConfigQuerySnafu)?
                .context(build_config_service_sources_error::MissingDirectiveSnafu {
                    directive: "server_name",
                })?;

            let has_cert = server
                .get::<gateway::parse::types::PathConfig>("ssl_certificate")
                .context(build_config_service_sources_error::ConfigQuerySnafu)?
                .is_some();
            let has_key = server
                .get::<gateway::parse::types::PathConfig>("ssl_certificate_key")
                .context(build_config_service_sources_error::ConfigQuerySnafu)?
                .is_some();
            if home.is_some() && !has_cert && !has_key && server_names.0.len() > 1 {
                return build_config_service_sources_error::AmbiguousImplicitTlsSnafu.fail();
            }

            if access_rules_uri.is_none()
                && let Some(uri) = server
                    .get::<gateway::parse::types::AccessRulesUri>("access_rules")
                    .context(build_config_service_sources_error::ConfigQuerySnafu)?
            {
                access_rules_uri = Some(uri.0.as_str().to_owned());
            }

            let dns_resolver_url = server
                .get::<gateway::parse::types::ResolverConfig>("dns")
                .context(build_config_service_sources_error::ConfigQuerySnafu)?
                .map(|resolver| resolver.0.clone());

            for configured_name in &server_names.0 {
                let server_name = configured_name.name.clone();
                if !seen_server_names.insert(server_name.clone()) {
                    return build_config_service_sources_error::DuplicateServerNameSnafu {
                        name: server_name.to_string(),
                    }
                    .fail();
                }

                let identity = load_identity_for_server(home, server, &server_name).await?;
                sources.push(ServerSource::PishooConfig(PishooConfigServiceSource {
                    name: server_name,
                    identity,
                    bind: listens.clone(),
                    dns_resolver_url: dns_resolver_url.clone(),
                    server_node: server.clone(),
                }));
            }
        }

        let ctx = PrepareContext::load_config_service(access_rules_uri.as_deref(), router_state)
            .await
            .context(build_config_service_sources_error::PrepareContextSnafu)?;

        Ok((sources, ctx))
    }

    pub async fn prepare(
        &self,
        ctx: &PrepareContext,
    ) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        let listen_request = ListenRequest {
            identity: self.identity.clone(),
            bind: self.bind.clone(),
            dns_resolver_url: self.dns_resolver_url.clone(),
        };

        let request_fingerprint = ListenRequestFingerprint {
            server_name: self.name.clone(),
            bind_debug: format!("{:?}", self.bind),
            identity_debug: compute_identity_fingerprint(&self.identity),
            dns_resolver_debug: self.dns_resolver_url.as_ref().map(|u| u.to_string()),
        };

        let listener_spec = ListenerSpec {
            request_fingerprint,
        };

        let service_generation = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let service = Arc::new(ServerService {
            h3_settings: ctx.h3_settings.clone(),
            access_rules: ctx.access_rules.clone(),
            router_state: ctx.router_state.clone(),
            server_node: self.server_node.clone(),
            access_log_dir: None,
            server_name: self.name.clone(),
        });

        Ok(PreparedServerUpdate {
            name: self.name.clone(),
            listen_request,
            listener_spec: listener_spec.clone(),
            service,
            fingerprint: ServerFingerprint {
                listener_spec,
                service_generation,
            },
        })
    }
}

fn listen_values(
    server: &ConfigNode,
) -> Result<Vec<Listens>, gateway::parse::error::ConfigQueryError> {
    Ok(server
        .get_all::<ListenConfig>("listen")?
        .into_iter()
        .flat_map(|node| node.0.clone())
        .collect())
}

async fn load_identity_for_server(
    home: Option<&DhttpHome>,
    server: &ConfigNode,
    server_name: &DhttpName<'static>,
) -> Result<dhttp::identity::Identity, BuildConfigServiceSourcesError> {
    let cert_path = server
        .get::<gateway::parse::types::PathConfig>("ssl_certificate")
        .context(build_config_service_sources_error::ConfigQuerySnafu)?;
    let key_path = server
        .get::<gateway::parse::types::PathConfig>("ssl_certificate_key")
        .context(build_config_service_sources_error::ConfigQuerySnafu)?;

    match (cert_path, key_path) {
        (Some(cert_path), Some(key_path)) => {
            load_identity_from_paths(server_name, &cert_path.0, &key_path.0).await
        }
        (None, None) => {
            let home = home.context(build_config_service_sources_error::MissingDirectiveSnafu {
                directive: "ssl_certificate",
            })?;
            let profile = home
                .resolve_identity_profile(server_name.clone())
                .await
                .context(build_config_service_sources_error::ResolveIdentitySnafu {
                    name: server_name.to_string(),
                })?;
            profile.load_identity().await.context(
                build_config_service_sources_error::LoadIdentitySnafu {
                    name: server_name.to_string(),
                },
            )
        }
        (None, Some(_)) => build_config_service_sources_error::MissingDirectiveSnafu {
            directive: "ssl_certificate",
        }
        .fail(),
        (Some(_), None) => build_config_service_sources_error::MissingDirectiveSnafu {
            directive: "ssl_certificate_key",
        }
        .fail(),
    }
}

async fn load_identity_from_paths(
    server_name: &DhttpName<'static>,
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<dhttp::identity::Identity, BuildConfigServiceSourcesError> {
    let cert_pem = tokio::fs::read(cert_path)
        .await
        .context(build_config_service_sources_error::ReadCertSnafu { path: cert_path })?;
    let key_pem = tokio::fs::read(key_path)
        .await
        .context(build_config_service_sources_error::ReadKeySnafu { path: key_path })?;
    let (certs, key) = crate::tls::validate_tls_material(&cert_pem, &key_pem)
        .context(build_config_service_sources_error::InvalidTlsSnafu)?;
    Ok(dhttp::identity::Identity::new(
        server_name.clone().into(),
        certs,
        key,
    ))
}

impl ServerSource {
    pub fn name(&self) -> &DhttpName<'static> {
        match self {
            Self::IdentityService(source) => &source.name,
            Self::PishooConfig(source) => &source.name,
            #[cfg(test)]
            Self::Fake(source) => &source.name,
        }
    }

    pub async fn prepare(
        &self,
        ctx: &PrepareContext,
    ) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        match self {
            Self::IdentityService(source) => source.prepare(ctx).await,
            Self::PishooConfig(source) => source.prepare(ctx).await,
            #[cfg(test)]
            Self::Fake(source) => source.prepare(),
        }
    }
}

#[cfg(test)]
impl FakeServerSource {
    pub(crate) fn prepare(&self) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        match &self.outcome {
            FakePrepareOutcome::Success {
                listener_spec,
                service_generation,
            } => Ok(PreparedServerUpdate {
                name: self.name.clone(),
                listen_request: Self::fake_listen_request(&self.name),
                listener_spec: listener_spec.clone(),
                service: Arc::new(ServerService::fake()),
                fingerprint: ServerFingerprint {
                    listener_spec: listener_spec.clone(),
                    service_generation: *service_generation,
                },
            }),
            FakePrepareOutcome::Failure => Err(PrepareServerUpdateError::SyntheticFailure {
                server_name: self.name.as_full().to_owned(),
            }),
        }
    }

    fn dhttp_subject_key_identifier_der() -> Vec<u8> {
        use dhttp::certificate::{
            CertificateChainKey, CertificateChainKind, CertificateSequence,
            DhttpSubjectKeyIdentifier, OwnerHash,
        };

        let owner_hash =
            OwnerHash::try_from("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .unwrap();
        let value = DhttpSubjectKeyIdentifier::new(
            CertificateChainKey::new(
                CertificateSequence::from(0u8),
                CertificateChainKind::Primary,
            ),
            owner_hash,
        )
        .to_string();

        let bytes = value.as_bytes();
        assert!(bytes.len() < 128, "test dhttp ski must use short-form DER");
        let mut der = Vec::with_capacity(bytes.len() + 2);
        der.push(0x04);
        der.push(bytes.len() as u8);
        der.extend_from_slice(bytes);
        der
    }

    fn fake_listen_request(name: &DhttpName<'static>) -> ListenRequest {
        use dhttp::identity::Identity;

        let fqdn = name.as_full().to_owned();
        let key_pair = rcgen::KeyPair::generate().expect("rcgen key generation");
        let mut params = rcgen::CertificateParams::new(vec![fqdn.clone()]).expect("rcgen params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, &fqdn);
        params
            .custom_extensions
            .push(rcgen::CustomExtension::from_oid_content(
                &[2, 5, 29, 14],
                Self::dhttp_subject_key_identifier_der(),
            ));
        let cert = params.self_signed(&key_pair).expect("rcgen self-sign");
        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der())
            .expect("rcgen key der");
        let identity = Identity::new(name.clone().into(), vec![cert_der], key_der);
        ListenRequest {
            identity,
            bind: vec![],
            dns_resolver_url: None,
        }
    }
}

#[cfg(test)]
impl ServerSource {
    pub(crate) fn fake_success(
        name: &str,
        service_generation: u64,
        listener_spec: ListenerSpec,
    ) -> Self {
        Self::Fake(FakeServerSource {
            name: fake_name(name),
            outcome: FakePrepareOutcome::Success {
                listener_spec,
                service_generation,
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
                server_name: fake_name(label),
                bind_debug: format!("bind:{label}"),
                identity_debug: format!("identity:{label}"),
                dns_resolver_debug: None,
            },
        }
    }
}

#[cfg(test)]
impl ServerFingerprint {
    pub(crate) fn generation_for_test(&self) -> u64 {
        self.service_generation
    }
}

#[cfg(test)]
fn fake_name(name: &str) -> DhttpName<'static> {
    DhttpName::try_from(name.to_owned()).expect("test name must be a valid dhttp name")
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildConfigServiceSourcesError {
    #[snafu(display("failed to canonicalize pishoo config server nodes"))]
    Canonicalize { source: gateway::error::Whatever },

    #[snafu(display("pishoo config service missing `{directive}`"))]
    MissingDirective { directive: &'static str },

    #[snafu(display("failed to read typed configuration value"))]
    ConfigQuery {
        source: gateway::parse::error::ConfigQueryError,
    },

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

    #[snafu(display("failed to resolve identity `{name}` in dhttp home"))]
    ResolveIdentity {
        name: String,
        source: dhttp::home::identity::ssl::ResolveIdentityProfileError,
    },

    #[snafu(display("failed to load identity `{name}` from dhttp home"))]
    LoadIdentity {
        name: String,
        source: dhttp::home::identity::ssl::LoadIdentityError,
    },

    #[snafu(display("implicit dhttp home TLS is ambiguous for multiple server_name values"))]
    AmbiguousImplicitTls,

    #[snafu(display("duplicate pishoo config server_name `{name}`"))]
    DuplicateServerName { name: String },

    #[snafu(display("pishoo config service missing `listen`"))]
    MissingListen,

    #[snafu(display("failed to prepare context"))]
    PrepareContext { source: BuildPrepareContextError },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildPrepareContextError {
    #[snafu(display("failed to load default policy bundle"))]
    Policy { source: crate::policy::PolicyError },
}

impl PrepareContext {
    pub async fn load_config_service(
        access_rules_uri: Option<&str>,
        router_state: RouterState,
    ) -> Result<Self, BuildPrepareContextError> {
        let policy_bundle = crate::policy::load_policy_bundle(access_rules_uri)
            .await
            .context(build_prepare_context_error::PolicySnafu)?;

        Ok(Self {
            h3_settings: h3_settings(),
            access_rules: policy_bundle.location_rules,
            router_state,
        })
    }

    pub async fn load_worker(
        _home: &DhttpHome,
        router_state: RouterState,
    ) -> Result<Self, BuildPrepareContextError> {
        let policy_bundle = crate::policy::load_policy_bundle(None)
            .await
            .context(build_prepare_context_error::PolicySnafu)?;

        Ok(Self {
            h3_settings: h3_settings(),
            access_rules: policy_bundle.location_rules,
            router_state,
        })
    }
}

impl IdentityServiceSource {
    pub async fn prepare(
        &self,
        ctx: &PrepareContext,
    ) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        use crate::worker::config::BuildConfigError;

        let identity = self
            .identity_profile
            .load_identity()
            .await
            .whatever_context::<_, BuildConfigError>("failed to load TLS material")
            .context(prepare_server_update_error::IdentityServiceSnafu)?;

        let conf_path = self.identity_profile.server_conf_path();
        let identity_server_nodes = if conf_path.is_file() {
            load_identity_servers(&self.home, &self.identity_profile)
                .await
                .whatever_context::<_, BuildConfigError>("failed to load identity config")
                .context(prepare_server_update_error::IdentityServiceSnafu)?
        } else {
            Vec::new()
        };

        let mut target_node = None;
        let mut target_binds = Vec::new();

        for server_node in &identity_server_nodes {
            let server_names = match server_node.get::<ServerNames>("server_name") {
                Ok(Some(sn)) => sn,
                _ => continue,
            };

            let matches = server_names.0.iter().any(|sn| sn.name == self.name);
            if matches {
                let listens: Vec<Listens> = server_node
                    .get_all::<ListenConfig>("listen")
                    .whatever_context::<_, BuildConfigError>("failed to read listen")
                    .context(prepare_server_update_error::IdentityServiceSnafu)?
                    .into_iter()
                    .flat_map(|listen| listen.0.clone())
                    .collect();
                if !listens.is_empty() {
                    target_binds = listens;
                    target_node = Some(server_node.clone());
                    break;
                }
            }
        }

        if target_binds.is_empty() {
            let error = <BuildConfigError as snafu::FromString>::without_source(format!(
                "no listen specifications found for server {}",
                self.name
            ));
            return Err(PrepareServerUpdateError::IdentityService { source: error });
        }

        let dns_resolver_url = target_node
            .as_ref()
            .and_then(|node| node.get::<ResolverConfig>("dns").ok().flatten())
            .map(|resolver| resolver.0.clone());

        let listen_request = ListenRequest {
            identity: identity.clone(),
            bind: target_binds.clone(),
            dns_resolver_url: dns_resolver_url.clone(),
        };

        let request_fingerprint = ListenRequestFingerprint {
            server_name: self.name.clone(),
            bind_debug: format!("{:?}", target_binds),
            identity_debug: crate::service::source::compute_identity_fingerprint(&identity),
            dns_resolver_debug: dns_resolver_url.map(|u| u.to_string()),
        };

        let listener_spec = ListenerSpec {
            request_fingerprint,
        };

        let service_generation = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Worker access control: load rules from identity server.conf if
        // access_rules is configured. Falls back to empty rules otherwise.
        let access_rules = if let Some(uri) = target_node
            .as_ref()
            .and_then(|node| node.get::<AccessRulesUri>("access_rules").ok().flatten())
        {
            let uri_str = uri.0.to_string();
            match crate::policy::load_policy_bundle(Some(&uri_str)).await {
                Ok(bundle) => bundle.location_rules,
                Err(error) => {
                    tracing::warn!(
                        error = %Report::from_error(&error),
                        uri = %uri_str,
                        "failed to load access rules from identity config"
                    );
                    ctx.access_rules.clone()
                }
            }
        } else {
            ctx.access_rules.clone()
        };

        let server_node = target_node.clone().unwrap_or_else(|| {
            let registry = gateway::parse::default_registry();
            let doc = gateway::parse::load_config_text(
                "",
                None,
                &registry,
                gateway::parse::registry::BuildOptions {
                    dhttp_home: None,
                    identity_profile: None,
                },
            )
            .unwrap();
            doc.root
        });

        let service = Arc::new(ServerService {
            h3_settings: ctx.h3_settings.clone(),
            access_rules,
            router_state: ctx.router_state.clone(),
            server_node,
            access_log_dir: Some(self.identity_profile.logs_dir()),
            server_name: self.name.clone(),
        });

        Ok(PreparedServerUpdate {
            name: self.name.clone(),
            listen_request,
            listener_spec: listener_spec.clone(),
            service,
            fingerprint: ServerFingerprint {
                listener_spec,
                service_generation,
            },
        })
    }
}

pub(crate) fn compute_identity_fingerprint(identity: &dhttp::identity::Identity) -> String {
    use sha2::{Digest, Sha256};
    let mut cert_digest = Sha256::new();
    for cert in identity.certs.iter() {
        cert_digest.update(cert.as_ref());
    }
    let cert_digest = cert_digest.finalize();

    let mut key_digest = Sha256::new();
    key_digest.update(identity.key.secret_der());
    let key_digest = key_digest.finalize();

    format!(
        "{}@{}@{}",
        identity.name(),
        hex_lower(cert_digest.as_ref()),
        hex_lower(key_digest.as_ref())
    )
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use dhttp::name::DhttpName;

    use super::*;

    #[test]
    fn fingerprint_incorporates_cert_and_key() {
        let name = DhttpName::try_from("test.dhttp.net".to_owned()).unwrap();
        let req1 = FakeServerSource::fake_listen_request(&name);
        let req2 = FakeServerSource::fake_listen_request(&name);

        let fp1 = compute_identity_fingerprint(&req1.identity);
        let fp2 = compute_identity_fingerprint(&req2.identity);

        assert_ne!(fp1, fp2, "Fingerprints must differ when cert/key differ");
        assert!(fp1.starts_with("test.dhttp.net@"));
    }

    #[cfg(feature = "sshd")]
    struct DummySpawner;
    #[cfg(feature = "sshd")]
    impl gateway::control_plane::DynSpawnSession for DummySpawner {
        fn spawn_session<'a>(
            &'a self,
            _username: &'a str,
        ) -> futures::future::BoxFuture<
            'a,
            Result<
                gateway::control_plane::SessionTransport,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            Box::pin(async { Err("dummy".into()) })
        }
    }
    #[cfg(feature = "sshd")]
    struct DummyScope;
    #[cfg(feature = "sshd")]
    impl gateway::reverse::router::DynTaskScope for DummyScope {
        fn token(&self) -> tokio_util::sync::CancellationToken {
            tokio_util::sync::CancellationToken::new()
        }
        fn spawn(&self, _task: futures::future::BoxFuture<'static, ()>) {}
    }

    fn dummy_router_state() -> gateway::reverse::router::RouterState {
        gateway::reverse::router::RouterState {
            #[cfg(feature = "sshd")]
            session_spawner: std::sync::Arc::new(DummySpawner),
            #[cfg(feature = "sshd")]
            task_scope: std::sync::Arc::new(DummyScope),
        }
    }

    #[test]
    fn server_source_uses_identity_service_variant_name() {
        let source = ServerSource::IdentityService(IdentityServiceSource {
            name: fake_name("identity-source.dhttp.net"),
            home: dhttp::home::DhttpHome::new(std::path::PathBuf::from("/tmp/home")),
            identity_profile: dhttp::home::DhttpHome::new(std::path::PathBuf::from("/tmp/home"))
                .identity_profile(fake_name("identity-source.dhttp.net")),
        });

        assert_eq!(source.name().as_full(), "identity-source.dhttp.net");
    }

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("pishoo-{label}-{nanos}"))
    }

    fn identity_pem(name: &dhttp::name::DhttpName<'static>) -> (String, String) {
        let fqdn = name.as_full().to_owned();
        let key_pair = rcgen::KeyPair::generate().expect("rcgen key generation");
        let mut params = rcgen::CertificateParams::new(vec![fqdn.clone()]).expect("rcgen params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, &fqdn);
        params
            .custom_extensions
            .push(rcgen::CustomExtension::from_oid_content(
                &[2, 5, 29, 14],
                FakeServerSource::dhttp_subject_key_identifier_der(),
            ));
        let cert = params.self_signed(&key_pair).expect("rcgen self-sign");
        (cert.pem(), key_pair.serialize_pem())
    }

    async fn write_identity(
        profile: &dhttp::home::identity::IdentityProfile,
        name: &dhttp::name::DhttpName<'static>,
    ) {
        let (cert_pem, key_pem) = identity_pem(name);
        profile
            .save_identity(cert_pem.as_bytes(), key_pem.as_bytes())
            .await
            .expect("save identity");
    }

    #[tokio::test]
    async fn pishoo_config_service_loads_identity_from_home_when_tls_is_omitted() {
        let home = dhttp::home::DhttpHome::new(unique_test_dir("config-service-home"));
        let name = fake_name("config-home.dhttp.net");
        let profile = home.identity_profile(name.clone());
        write_identity(&profile, &name).await;

        let config = gateway::parse::load_config_text(
            "pishoo { server { listen all 443; server_name config-home.dhttp.net; } }",
            Some(home.as_path()),
            &gateway::parse::default_registry(),
            gateway::parse::registry::BuildOptions {
                dhttp_home: Some(&home),
                identity_profile: None,
            },
        )
        .expect("parse config with home context");
        let pishoo = config.root.children("pishoo").unwrap()[0].clone();
        let server = pishoo.children("server").unwrap()[0].clone();

        let (sources, _ctx) =
            PishooConfigServiceSource::load_all(&[server], Some(&home), dummy_router_state())
                .await
                .expect("load config service sources");

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name().as_full(), "config-home.dhttp.net");
    }

    #[tokio::test]
    async fn explicit_config_service_rejects_omitted_tls() {
        let config = gateway::parse::parse_config_str_for_test(
            "pishoo { server { listen all 443; server_name explicit-missing.dhttp.net; } }",
        )
        .expect_err("explicit config parse should reject missing TLS");

        let report = snafu::Report::from_error(&config.error).to_string();
        assert!(report.contains("missing ssl_certificate directive"));
    }

    #[cfg(feature = "sshd")]
    #[test]
    fn h3_settings_advertise_webtransport_support_when_sshd_enabled() {
        let settings = h3_settings();

        assert!(settings.enable_connect_protocol());
        assert!(settings.enable_webtransport());
        assert!(settings.webtransport_flow_control_enabled());
    }

    #[tokio::test]
    async fn local_server_source_prepare_produces_listener_spec_with_cert_fingerprint() {
        let name = DhttpName::try_from("test.dhttp.net".to_owned()).unwrap();
        let req1 = FakeServerSource::fake_listen_request(&name);

        let config = gateway::parse::parse_config_str_for_test(
            "server { listen 127.0.0.1:443; server_name localhost; ssl_certificate /tmp/a; ssl_certificate_key /tmp/b; }",
        )
        .unwrap();
        let server_node = config.root.children("server").unwrap()[0].clone();

        let source = PishooConfigServiceSource {
            name: name.clone(),
            identity: req1.identity.clone(),
            bind: req1.bind.clone(),
            dns_resolver_url: req1.dns_resolver_url.clone(),
            server_node,
        };

        let ctx = PrepareContext {
            h3_settings: std::sync::Arc::new(dhttp::h3x::dhttp::settings::Settings::default()),
            access_rules: std::sync::Arc::new(
                dhttp::access::matcher::LocationRulesMatcher::default(),
            ),
            router_state: dummy_router_state(),
        };

        let prepared = source.prepare(&ctx).await.expect("prepare success");

        let identity_debug = &prepared.listener_spec.request_fingerprint.identity_debug;
        assert!(
            identity_debug.starts_with("test.dhttp.net@"),
            "fingerprint should contain name and separator: {}",
            identity_debug
        );
        assert!(identity_debug.len() > 20, "fingerprint should contain hash");
    }

    #[tokio::test]
    async fn local_server_source_prepare_two_rotated_certs_produce_distinct_fingerprints() {
        let name = DhttpName::try_from("rotated.dhttp.net".to_owned()).unwrap();
        let req1 = FakeServerSource::fake_listen_request(&name);
        let req2 = FakeServerSource::fake_listen_request(&name);

        let config = gateway::parse::parse_config_str_for_test(
            "server { listen 127.0.0.1:443; server_name localhost; ssl_certificate /tmp/a; ssl_certificate_key /tmp/b; }",
        )
        .unwrap();
        let server_node = config.root.children("server").unwrap()[0].clone();

        let source1 = PishooConfigServiceSource {
            name: name.clone(),
            identity: req1.identity.clone(),
            bind: req1.bind.clone(),
            dns_resolver_url: req1.dns_resolver_url.clone(),
            server_node: server_node.clone(),
        };

        let source2 = PishooConfigServiceSource {
            name: name.clone(),
            identity: req2.identity.clone(),
            bind: req2.bind.clone(),
            dns_resolver_url: req2.dns_resolver_url.clone(),
            server_node: server_node.clone(),
        };

        let ctx = PrepareContext {
            h3_settings: std::sync::Arc::new(dhttp::h3x::dhttp::settings::Settings::default()),
            access_rules: std::sync::Arc::new(
                dhttp::access::matcher::LocationRulesMatcher::default(),
            ),
            router_state: dummy_router_state(),
        };

        let prepared1 = source1.prepare(&ctx).await.expect("prepare success");
        let prepared2 = source2.prepare(&ctx).await.expect("prepare success");

        assert_ne!(
            prepared1.listener_spec.request_fingerprint.identity_debug,
            prepared2.listener_spec.request_fingerprint.identity_debug,
            "Rotated certs should produce distinct fingerprints"
        );
    }
}

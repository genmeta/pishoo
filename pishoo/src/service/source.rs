use std::sync::Arc;

use dhttp::name::DhttpName;
use dhttp_home::{DhttpHome, identity::IdentityProfile};
use gateway::{
    control_plane::ListenRequest,
    parse::{
        document::ConfigNode,
        types::{ListenConfig, Listens, ResolverConfig, ServerIdConfig, ServerNames},
    },
    reverse::router::RouterState,
};
use snafu::{ResultExt, Snafu};

use super::snapshot::ServerService;
use crate::config::load_identity_servers;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenRequestFingerprint {
    pub server_name: DhttpName<'static>,
    pub bind_debug: String,
    pub identity_debug: String,
    pub dns_resolver_debug: Option<String>,
    pub publish_server_id: Option<u8>,
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
    #[snafu(display("failed to build worker server config"))]
    Worker {
        source: crate::worker::config::BuildConfigError,
    },
    #[snafu(display("failed to build local server config"))]
    Local {
        source: crate::hypervisor::local_service::BuildLocalServiceError,
    },
    #[cfg(test)]
    #[snafu(display("synthetic prepare failure for {server_name}"))]
    SyntheticFailure { server_name: String },
}

pub enum ServerSource {
    Worker(WorkerServerSource),
    Local(LocalServerSource),
    #[cfg(test)]
    Fake(FakeServerSource),
}

pub struct WorkerPrepareContext {
    pub h3_settings: Arc<h3x::dhttp::settings::Settings>,
    pub access_rules: Arc<dhttp_access::db::base::matcher::LocationRulesMatcher>,
    pub router_state: gateway::reverse::router::RouterState,
}

pub struct WorkerServerSource {
    pub name: DhttpName<'static>,
    pub identity_profile: IdentityProfile,
}

pub struct LocalServerSource {
    pub server_node: Arc<ConfigNode>,
    pub name: DhttpName<'static>,
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

impl ServerSource {
    pub fn name(&self) -> &DhttpName<'static> {
        match self {
            Self::Worker(source) => &source.name,
            Self::Local(source) => &source.name,
            #[cfg(test)]
            Self::Fake(source) => &source.name,
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

    fn fake_listen_request(name: &DhttpName<'static>) -> ListenRequest {
        use dhttp::{ddns::PublishOptions, identity::Identity};

        let fqdn = name.as_full().to_owned();
        let key_pair = rcgen::KeyPair::generate().expect("rcgen key generation");
        let mut params = rcgen::CertificateParams::new(vec![fqdn.clone()]).expect("rcgen params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, &fqdn);
        let cert = params.self_signed(&key_pair).expect("rcgen self-sign");
        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der())
            .expect("rcgen key der");
        let identity = Identity::new(name.clone().into(), vec![cert_der], key_der);
        ListenRequest {
            identity,
            bind: vec![],
            dns_resolver_url: None,
            publish_options: PublishOptions::default(),
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
                publish_server_id: None,
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
pub enum BuildContextError {
    #[snafu(display("failed to load default policy bundle"))]
    Policy { source: crate::policy::PolicyError },
}

impl WorkerPrepareContext {
    pub async fn load(
        _home: &DhttpHome,
        router_state: RouterState,
    ) -> Result<Self, BuildContextError> {
        let policy_bundle = crate::policy::load_policy_bundle(None)
            .await
            .context(build_context_error::PolicySnafu)?;

        Ok(Self {
            h3_settings: Arc::new(h3x::dhttp::settings::Settings::default()),
            access_rules: policy_bundle.location_rules,
            router_state,
        })
    }
}

impl WorkerServerSource {
    pub async fn prepare(
        &self,
        ctx: &WorkerPrepareContext,
    ) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        use dhttp::ddns::PublishOptions;

        use crate::worker::config::BuildConfigError;

        let identity = self
            .identity_profile
            .load_identity()
            .await
            .whatever_context::<_, BuildConfigError>("failed to load TLS material")
            .context(prepare_server_update_error::WorkerSnafu)?;

        let conf_path = self.identity_profile.server_conf_path();
        let identity_server_nodes = if conf_path.is_file() {
            load_identity_servers(&self.identity_profile)
                .await
                .whatever_context::<_, BuildConfigError>("failed to load identity config")
                .context(prepare_server_update_error::WorkerSnafu)?
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
                    .context(prepare_server_update_error::WorkerSnafu)?
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
            return Err(PrepareServerUpdateError::Worker { source: error });
        }

        let dns_resolver_url = target_node
            .as_ref()
            .and_then(|node| node.get::<ResolverConfig>("dns").ok().flatten())
            .map(|resolver| resolver.0.clone());

        let publish_server_id = target_node
            .as_ref()
            .and_then(|node| node.get::<ServerIdConfig>("server_id").ok().flatten())
            .map(|id| id.0);

        let publish_options = PublishOptions {
            server_id: publish_server_id,
        };

        let listen_request = ListenRequest {
            identity: identity.clone(),
            bind: target_binds.clone(),
            dns_resolver_url: dns_resolver_url.clone(),
            publish_options,
        };

        let request_fingerprint = ListenRequestFingerprint {
            server_name: self.name.clone(),
            bind_debug: format!("{:?}", target_binds),
            identity_debug: format!("{:?}", identity.name()),
            dns_resolver_debug: dns_resolver_url.map(|u| u.to_string()),
            publish_server_id,
        };

        let listener_spec = ListenerSpec {
            request_fingerprint,
        };

        let server_node = target_node.unwrap_or_else(|| {
            let registry = gateway::parse::default_registry();
            let doc = gateway::parse::load_config_text(
                "",
                None,
                &registry,
                gateway::parse::registry::BuildOptions {
                    identity_profile: None,
                },
            )
            .unwrap();
            doc.root
        });

        let service_generation = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let service = Arc::new(ServerService {
            h3_settings: ctx.h3_settings.clone(),
            access_rules: ctx.access_rules.clone(),
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

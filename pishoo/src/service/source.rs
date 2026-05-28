#![allow(dead_code)]

use std::{path::PathBuf, sync::Arc};

use dhttp::name::DhttpName;
use gateway::{control_plane::ListenRequest, parse::document::ConfigNode};
use snafu::Snafu;

use super::snapshot::ServerService;

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
    pub listen_request: Option<ListenRequest>,
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

pub struct WorkerServerSource {
    pub dhttp_home: PathBuf,
    pub name: DhttpName<'static>,
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

    pub async fn prepare(&self) -> Result<PreparedServerUpdate, PrepareServerUpdateError> {
        match self {
            Self::Worker(_) | Self::Local(_) => {
                unimplemented!("wired into WorkerRuntime in Task 7")
            }
            #[cfg(test)]
            Self::Fake(source) => source.prepare(),
        }
    }
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
                listen_request: None,
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

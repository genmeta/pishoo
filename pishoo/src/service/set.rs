use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use dhttp::name::DhttpName;
use gateway::{
    parse::domain::ResolvedConfigPath,
    reverse::log::{AccessLogOutput, OpenAccessLogOutputError},
};

use super::{
    accept::{AcceptState, DrainOutcome},
    resource::{AccessLogResourcePlan, AccessLogResources, ServerResources},
};

pub struct ResourceSet<L> {
    pub(crate) servers: HashMap<DhttpName<'static>, ServerResources<L>>,
    pub(crate) access_output_cache: HashMap<ResolvedConfigPath, Weak<AccessLogOutput>>,
}

impl<L> Default for ResourceSet<L> {
    fn default() -> Self {
        Self {
            servers: HashMap::new(),
            access_output_cache: HashMap::new(),
        }
    }
}

impl<L> ResourceSet<L> {
    pub(crate) fn acquire_output(
        &mut self,
        path: ResolvedConfigPath,
    ) -> Result<Arc<AccessLogOutput>, OpenAccessLogOutputError> {
        if let Some(output) = self.access_output_cache.get(&path).and_then(Weak::upgrade) {
            return Ok(output);
        }

        let output = Arc::new(AccessLogOutput::open(path.clone())?);
        self.access_output_cache
            .insert(path, Arc::downgrade(&output));
        Ok(output)
    }

    pub(crate) fn acquire_access_logs(
        &mut self,
        plan: AccessLogResourcePlan,
    ) -> Result<AccessLogResources, OpenAccessLogOutputError> {
        let server = self.acquire_configured_output(plan.server)?;
        let locations = plan
            .locations
            .into_vec()
            .into_iter()
            .map(|config| self.acquire_configured_output(config))
            .collect::<Result<Box<[_]>, _>>()?;
        Ok(AccessLogResources { server, locations })
    }

    fn acquire_configured_output(
        &mut self,
        config: gateway::parse::config::ResolvedAccessLogConfig,
    ) -> Result<Option<Arc<AccessLogOutput>>, OpenAccessLogOutputError> {
        match config {
            gateway::parse::config::ResolvedAccessLogConfig::Disabled => Ok(None),
            gateway::parse::config::ResolvedAccessLogConfig::Enabled(path) => {
                self.acquire_output(path).map(Some)
            }
        }
    }
}

pub(crate) struct ServiceCompletion {
    pub(crate) name: DhttpName<'static>,
    marker: Arc<()>,
}

#[derive(Clone)]
struct ServiceCompletionToken {
    name: DhttpName<'static>,
    marker: Arc<()>,
}

impl ServiceCompletionToken {
    fn new(name: DhttpName<'static>) -> Self {
        Self {
            name,
            marker: Arc::new(()),
        }
    }

    fn completion(&self) -> ServiceCompletion {
        ServiceCompletion {
            name: self.name.clone(),
            marker: Arc::clone(&self.marker),
        }
    }

    fn owns(&self, completion: &ServiceCompletion) -> bool {
        Arc::ptr_eq(&self.marker, &completion.marker)
    }
}

pub struct ServerServiceHandle<L> {
    accept: AcceptState<L>,
    completion: ServiceCompletionToken,
}

impl<L> ServerServiceHandle<L>
where
    L: Send + 'static,
{
    pub(crate) fn start(
        name: DhttpName<'static>,
        listener: L,
        service: std::sync::Arc<super::snapshot::ServerService>,
        completed: tokio::sync::mpsc::UnboundedSender<ServiceCompletion>,
    ) -> Self
    where
        L: dhttp::h3x::quic::Listen,
        L::Error: Send,
        L::Connection: Send + 'static,
        <L::Connection as dhttp::h3x::quic::WithLocalAuthority>::LocalAuthority: Send + Sync,
        <L::Connection as dhttp::h3x::quic::WithRemoteAuthority>::RemoteAuthority: Send + Sync,
    {
        let completion = ServiceCompletionToken::new(name);
        let task_completion = completion.clone();
        Self {
            accept: AcceptState::start(listener, service, move || {
                let _ = completed.send(task_completion.completion());
            }),
            completion,
        }
    }

    pub async fn drain(self) -> DrainOutcome<L> {
        self.accept.drain().await
    }
    pub fn is_finished(&self) -> bool {
        self.accept.is_finished()
    }

    pub(crate) fn owns_completion(&self, completion: &ServiceCompletion) -> bool {
        self.completion.owns(completion)
    }
}

pub struct ServiceSet<L> {
    pub(crate) servers: HashMap<DhttpName<'static>, ServerServiceHandle<L>>,
    completed_tx: tokio::sync::mpsc::UnboundedSender<ServiceCompletion>,
    completed_rx: tokio::sync::mpsc::UnboundedReceiver<ServiceCompletion>,
}

impl<L> Default for ServiceSet<L> {
    fn default() -> Self {
        let (completed_tx, completed_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            servers: HashMap::new(),
            completed_tx,
            completed_rx,
        }
    }
}

impl<L> ServiceSet<L> {
    pub(crate) fn completion_sender(
        &self,
    ) -> tokio::sync::mpsc::UnboundedSender<ServiceCompletion> {
        self.completed_tx.clone()
    }

    pub(crate) async fn next_completed(&mut self) -> ServiceCompletion {
        self.completed_rx
            .recv()
            .await
            .expect("service set owns the completion sender")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gateway::parse::domain::ResolvedConfigPath;

    use super::*;

    #[test]
    fn completion_only_belongs_to_the_service_instance_that_created_it() {
        let name = "alice.dhttp.net".parse().unwrap();
        let first = ServiceCompletionToken::new(name);
        let replacement = ServiceCompletionToken::new("alice.dhttp.net".parse().unwrap());
        let completion = first.completion();

        assert!(first.owns(&completion));
        assert!(!replacement.owns(&completion));
    }

    #[test]
    fn same_process_path_reuses_output_and_last_binding_closes_it() {
        let mut resources = ResourceSet::<()>::default();
        let path = ResolvedConfigPath::try_from(std::env::temp_dir().join(format!(
                "pishoo-shared-access-{}-{}.log",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )))
        .unwrap();

        let first = resources.acquire_output(path.clone()).unwrap();
        let second = resources.acquire_output(path.clone()).unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        drop(first);
        drop(second);
        assert!(
            resources
                .access_output_cache
                .get(&path)
                .unwrap()
                .upgrade()
                .is_none()
        );

        let _ = std::fs::remove_file(PathBuf::from(path.as_ref()));
    }

    #[test]
    fn output_open_failure_does_not_create_a_binding() {
        let mut resources = ResourceSet::<()>::default();
        let path = ResolvedConfigPath::try_from(PathBuf::from("/proc/pishoo/access.log")).unwrap();

        assert!(resources.acquire_output(path).is_err());
        assert!(resources.access_output_cache.is_empty());
    }
}

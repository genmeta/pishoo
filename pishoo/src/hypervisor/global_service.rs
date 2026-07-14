//! Global pishoo service management.
//!
//! Spawns and reloads global identity services and pishoo config services using
//! the [`RuntimeRegistry`] model.

use std::sync::Arc;

use crate::{
    hypervisor::{in_process_plane::InProcessControlPlane, state::RootState},
    service::{
        runtime::RuntimeRegistry,
        source::{PrepareContext, ServerSource, TypedServerSource},
    },
};

/// Handle to a running global service, used for shutdown and replacement.
pub struct GlobalServiceHandle {
    registry: Option<RuntimeRegistry<InProcessControlPlane>>,
}

impl GlobalServiceHandle {
    /// Cancel the running service and wait for listener shutdown.
    pub async fn shutdown(mut self) {
        if let Some(registry) = self.registry.take() {
            registry.shutdown().await;
        }
    }

    pub async fn wait_service_completion(&mut self) -> dhttp::name::DhttpName<'static> {
        self.registry
            .as_mut()
            .expect("a live global service handle owns its registry")
            .wait_service_completion()
            .await
    }

    pub async fn handle_service_exit(&mut self, name: dhttp::name::DhttpName<'static>) {
        self.registry
            .as_mut()
            .expect("a live global service handle owns its registry")
            .handle_service_exit(name)
            .await;
    }
}

pub async fn wait_global_service_completion(
    handle: &mut Option<GlobalServiceHandle>,
) -> dhttp::name::DhttpName<'static> {
    match handle {
        Some(handle) => handle.wait_service_completion().await,
        None => std::future::pending().await,
    }
}

impl Drop for GlobalServiceHandle {
    fn drop(&mut self) {
        // Explicit shutdown() remains the authoritative cleanup path because
        // listener shutdown is async. If the handle is dropped unexpectedly,
        // listener handles release through RootState's async resource
        // transition path, and RootState::cleanup_local_resources() remains
        // responsible for retiring any in-process resources still registered.
    }
}

async fn build_global_sources(
    plan: &crate::config::GlobalPishooPlan,
    router_state: gateway::reverse::router::RouterState,
) -> (Vec<ServerSource>, PrepareContext) {
    let mut configs = Vec::new();
    for candidate in plan.direct_servers() {
        match candidate.result() {
            Ok(server) => configs.push(Arc::new(server.clone())),
            Err(error) => {
                tracing::warn!(error = %snafu::Report::from_error(error), "direct server config rejected")
            }
        }
    }
    if let Some(home) = plan.home() {
        match crate::config::load_identity_server_candidates(home, plan.worker_defaults()).await {
            Ok(candidates) => {
                for candidate in candidates.into_vec() {
                    let (profile, result) = candidate.into_parts();
                    match result {
                        Ok(Some(server)) => configs.push(Arc::new(server)),
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(profile = ?profile.map(|p| p.name().to_string()), error = %snafu::Report::from_error(&error), "identity server config rejected")
                        }
                    }
                }
            }
            Err(error) => tracing::warn!(
                error = %snafu::Report::from_error(&error),
                "global identity service discovery failed"
            ),
        }
    }
    TypedServerSource::load_all(configs, router_state).await
}

/// Spawn the global service from configuration. Returns `None` if no global
/// services are configured.
pub async fn spawn_global_service(
    state: &Arc<RootState>,
    plan: &crate::config::GlobalPishooPlan,
) -> Option<GlobalServiceHandle> {
    let plane = Arc::new(InProcessControlPlane::new(state.clone()));

    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
        #[cfg(feature = "sshd")]
        task_scope: Arc::new(state.local_task_scope()),
    };

    let (sources, ctx) = build_global_sources(plan, router_state).await;

    if sources.is_empty() {
        tracing::debug!("no global services configured");
        return None;
    }

    let service_count = sources.len();
    let mut registry = RuntimeRegistry::new(plane);
    registry.apply_sources(sources, &ctx).await;

    tracing::info!(services = service_count, "global service started");

    Some(GlobalServiceHandle {
        registry: Some(registry),
    })
}

/// Replace the running global service with a freshly built one.
pub async fn replace_global_service(
    state: &Arc<RootState>,
    handle: &mut Option<GlobalServiceHandle>,
    plan: &crate::config::GlobalPishooPlan,
) {
    let plane = Arc::new(InProcessControlPlane::new(state.clone()));

    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
        #[cfg(feature = "sshd")]
        task_scope: Arc::new(state.local_task_scope()),
    };

    let (sources, ctx) = build_global_sources(plan, router_state).await;

    if sources.is_empty() {
        if let Some(old) = handle.take() {
            old.shutdown().await;
        }
        return;
    }

    if let Some(registry) = handle
        .as_mut()
        .and_then(|existing| existing.registry.as_mut())
    {
        registry.apply_sources(sources, &ctx).await;
        return;
    }

    let mut new_registry = RuntimeRegistry::new(plane);
    new_registry.apply_sources(sources, &ctx).await;
    if let Some(old) = handle.take() {
        old.shutdown().await;
    }
    *handle = Some(GlobalServiceHandle {
        registry: Some(new_registry),
    });
}

//! Global pishoo service management.
//!
//! Spawns and reloads global identity services and pishoo config services using
//! the [`RuntimeRegistry`] model.

use std::sync::Arc;

use snafu::{ResultExt, Snafu};

use crate::{
    hypervisor::{in_process_plane::InProcessControlPlane, state::RootState},
    service::{
        runtime::RuntimeRegistry,
        source::{PishooConfigServiceSource, PrepareContext, ServerSource},
    },
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildGlobalSourcesError {
    #[snafu(display("failed to load identity services"))]
    IdentityServices { source: crate::worker::config::BuildConfigError },
    #[snafu(display("failed to load pishoo config services"))]
    ConfigServices { source: crate::service::source::BuildConfigServiceSourcesError },
    #[snafu(display("failed to prepare global service context"))]
    PrepareContext { source: crate::service::source::BuildPrepareContextError },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SpawnGlobalServiceError {
    #[snafu(display("failed to build global service sources"))]
    Build { source: BuildGlobalSourcesError },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ReplaceGlobalServiceError {
    #[snafu(display("failed to build global service sources"))]
    Build { source: BuildGlobalSourcesError },
}

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
    source: &crate::config::PishooConfigSource,
    entry_config: &crate::config::EntryConfig,
    router_state: gateway::reverse::router::RouterState,
) -> Result<(Vec<ServerSource>, PrepareContext), BuildGlobalSourcesError> {
    let mut sources = Vec::new();
    let mut ctx = None;

    if source.load_identity_services() {
        let home = source.dhttp_home().expect("global source has dhttp home");
        let identity_sources = crate::worker::config::load_identity_service_sources(home)
            .await
            .context(build_global_sources_error::IdentityServicesSnafu)?;
        sources.extend(
            identity_sources
                .into_iter()
                .map(ServerSource::IdentityService),
        );
    }

    if !entry_config.local_servers.is_empty() {
        let (config_sources, config_ctx) = PishooConfigServiceSource::load_all(
            &entry_config.local_servers,
            source.dhttp_home(),
            router_state.clone(),
        )
        .await
        .context(build_global_sources_error::ConfigServicesSnafu)?;
        sources.extend(config_sources);
        ctx = Some(config_ctx);
    }

    let ctx = match ctx {
        Some(ctx) => ctx,
        None => {
            let Some(home) = source.dhttp_home() else {
                return Ok((sources, PrepareContext::load_config_service(None, router_state)
                    .await
                    .context(build_global_sources_error::PrepareContextSnafu)?));
            };
            PrepareContext::load_worker(home, router_state)
                .await
                .context(build_global_sources_error::PrepareContextSnafu)?
        }
    };

    Ok((sources, ctx))
}

/// Spawn the global service from configuration. Returns `None` if no global
/// services are configured.
pub async fn spawn_global_service(
    state: &Arc<RootState>,
    source: &crate::config::PishooConfigSource,
    entry_config: &crate::config::EntryConfig,
) -> Result<Option<GlobalServiceHandle>, SpawnGlobalServiceError> {
    let plane = Arc::new(InProcessControlPlane::new(state.clone()));

    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
        #[cfg(feature = "sshd")]
        task_scope: Arc::new(state.local_task_scope()),
    };

    let (sources, ctx) = build_global_sources(source, entry_config, router_state)
        .await
        .context(spawn_global_service_error::BuildSnafu)?;

    if sources.is_empty() {
        tracing::debug!("no global services configured");
        return Ok(None);
    }

    let service_count = sources.len();
    let mut registry = RuntimeRegistry::new(plane);
    registry.apply_sources(sources, &ctx).await;

    tracing::info!(services = service_count, "global service started");

    Ok(Some(GlobalServiceHandle {
        registry: Some(registry),
    }))
}

/// Replace the running global service with a freshly built one.
pub async fn replace_global_service(
    state: &Arc<RootState>,
    handle: &mut Option<GlobalServiceHandle>,
    source: &crate::config::PishooConfigSource,
    entry_config: &crate::config::EntryConfig,
) -> Result<(), ReplaceGlobalServiceError> {
    let plane = Arc::new(InProcessControlPlane::new(state.clone()));

    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
        #[cfg(feature = "sshd")]
        task_scope: Arc::new(state.local_task_scope()),
    };

    let (sources, ctx) = build_global_sources(source, entry_config, router_state)
        .await
        .context(replace_global_service_error::BuildSnafu)?;

    if sources.is_empty() {
        if let Some(old) = handle.take() {
            old.shutdown().await;
        }
        return Ok(());
    }

    if let Some(registry) = handle.as_mut().and_then(|existing| existing.registry.as_mut()) {
        registry.apply_sources(sources, &ctx).await;
        return Ok(());
    }

    let mut new_registry = RuntimeRegistry::new(plane);
    new_registry.apply_sources(sources, &ctx).await;
    if let Some(old) = handle.take() {
        old.shutdown().await;
    }
    *handle = Some(GlobalServiceHandle {
        registry: Some(new_registry),
    });

    Ok(())
}

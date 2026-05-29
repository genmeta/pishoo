//! Root-local service management.
//!
//! Spawns and reloads root-local servers (e.g. gateway metrics) using the
//! [`RuntimeRegistry`] model.

use std::sync::Arc;

use snafu::{ResultExt, Snafu};

use crate::{
    hypervisor::{local_plane::LocalControlPlane, state::RootState},
    service::{runtime::RuntimeRegistry, source::LocalServerSource},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SpawnLocalServiceError {
    #[snafu(display("failed to load local server sources"))]
    Load {
        source: crate::service::source::BuildLocalSourcesError,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ReplaceLocalServiceError {
    #[snafu(display("failed to load local server sources"))]
    Load {
        source: crate::service::source::BuildLocalSourcesError,
    },
}

/// Handle to a running root-local service, used for shutdown and replacement.
pub struct LocalServiceHandle {
    registry: Option<RuntimeRegistry<LocalControlPlane>>,
}

impl LocalServiceHandle {
    /// Cancel the running service and wait for listener shutdown.
    pub async fn shutdown(mut self) {
        if let Some(registry) = self.registry.take() {
            registry.shutdown().await;
        }
    }
}

impl Drop for LocalServiceHandle {
    fn drop(&mut self) {
        // RuntimeRegistry's ServerRuntime members each own AbortOnDropHandle
        // for their accept loops; dropping the registry aborts them. Listener
        // shutdown still requires the async shutdown() above for explicit
        // cleanup, but Drop is safe (no panic, no leak of accept tasks).
    }
}

/// Spawn the root-local service from configuration. Returns `None` if no
/// local servers are configured.
pub async fn spawn_local_service(
    state: &Arc<RootState>,
    entry_config: &crate::config::EntryConfig,
) -> Result<Option<LocalServiceHandle>, SpawnLocalServiceError> {
    if entry_config.local_servers.is_empty() {
        tracing::debug!("no local servers configured");
        return Ok(None);
    }

    let plane = Arc::new(LocalControlPlane::new(state.clone()));

    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
        #[cfg(feature = "sshd")]
        task_scope: Arc::new(state.local_task_scope()),
    };

    let (sources, ctx) = LocalServerSource::load_all(&entry_config.local_servers, router_state)
        .await
        .context(spawn_local_service_error::LoadSnafu)?;

    let server_count = sources.len();

    let mut registry = RuntimeRegistry::new(plane);
    registry.apply_sources(sources, &ctx).await;

    tracing::info!(servers = server_count, "local service started");

    Ok(Some(LocalServiceHandle {
        registry: Some(registry),
    }))
}

/// Replace the running root-local service with a freshly built one.
///
/// Prepares the new source set before mutating the old handle, then reuses
/// the existing registry when present so listener diffs stay in one runtime.
pub async fn replace_local_service(
    state: &Arc<RootState>,
    handle: &mut Option<LocalServiceHandle>,
    entry_config: &crate::config::EntryConfig,
) -> Result<(), ReplaceLocalServiceError> {
    if entry_config.local_servers.is_empty() {
        if let Some(old) = handle.take() {
            old.shutdown().await;
        }
        return Ok(());
    }

    let plane = Arc::new(LocalControlPlane::new(state.clone()));

    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
        #[cfg(feature = "sshd")]
        task_scope: Arc::new(state.local_task_scope()),
    };

    let (sources, ctx) = LocalServerSource::load_all(&entry_config.local_servers, router_state)
        .await
        .context(replace_local_service_error::LoadSnafu)?;

    if let Some(registry) = handle
        .as_mut()
        .and_then(|existing| existing.registry.as_mut())
    {
        registry.apply_sources(sources, &ctx).await;
        return Ok(());
    }

    let mut new_registry = RuntimeRegistry::new(plane);
    new_registry.apply_sources(sources, &ctx).await;
    if let Some(old) = handle.take() {
        old.shutdown().await;
    }
    *handle = Some(LocalServiceHandle {
        registry: Some(new_registry),
    });

    Ok(())
}

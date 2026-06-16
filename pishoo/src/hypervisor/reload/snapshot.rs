//! Configuration reload helpers for the root process.

use std::path::Path;

use gateway::error::Whatever;
use snafu::{FromString, ResultExt};

/// Snapshot of root-level configuration loaded during a reload preflight.
pub struct RootReloadSnapshot {
    pub entry_config: crate::config::EntryConfig,
    pub worker_targets: Vec<crate::config::ResolvedWorkerTarget>,
}

/// Load and validate the root configuration from disk.
///
/// Used during SIGHUP reload to preflight the new configuration before
/// applying any changes. Returns the parsed entry config and resolved
/// worker targets.
pub async fn load_root_reload_snapshot(config_file: &Path) -> Result<RootReloadSnapshot, Whatever> {
    let registry = gateway::parse::default_registry();
    let config = match gateway::parse::load_config_file(
        config_file,
        &registry,
        gateway::parse::registry::BuildOptions::default(),
    )
    .await
    {
        Ok(config) => config,
        Err(failure) => {
            tracing::warn!(
                error = %snafu::Report::from_error(&failure.error),
                diagnostic = %failure.diagnostic(),
                "failed to reload configuration"
            );
            return Err(Whatever::with_source(
                Box::new(failure),
                "failed to reload configuration".to_owned(),
            ));
        }
    };
    let entry_config = crate::config::parse_entry_config(&config.root)
        .whatever_context("failed to parse pishoo entry configuration")?;
    let worker_targets = crate::config::resolve_entry_worker_targets(&entry_config)
        .whatever_context("failed to resolve configured worker users during reload")?;

    Ok(RootReloadSnapshot {
        entry_config,
        worker_targets,
    })
}

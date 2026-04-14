//! Configuration reload helpers for the root process.

use std::path::Path;

use gateway::error::Whatever;
use snafu::ResultExt;
use tokio::fs;

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
    let config = fs::read(config_file).await.whatever_context(format!(
        "failed to read configuration file at `{}`",
        config_file.display()
    ))?;
    let config = gateway::parse::parse(&config, config_file.parent()).whatever_context(format!(
        "failed to parse configuration file at `{}`",
        config_file.display()
    ))?;
    let entry_config = crate::config::parse_entry_config(&config)
        .whatever_context("failed to parse pishoo entry configuration")?;
    let worker_targets = crate::config::resolve_entry_worker_targets(&entry_config)
        .whatever_context("failed to resolve configured worker users during reload")?;

    Ok(RootReloadSnapshot {
        entry_config,
        worker_targets,
    })
}

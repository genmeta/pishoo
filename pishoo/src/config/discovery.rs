use std::{path::Path, sync::Arc};

use gateway::{
    error::Whatever,
    parse::{Node, Value},
};
use genmeta_home::GenmetaHome;
use snafu::{ResultExt, whatever};

use super::{EntryConfig, ResolvedWorkerTarget, resolve_entry_worker_targets};
use crate::naming::canonicalize_server_nodes;

pub async fn discover_entry_servers(
    entry_config: &EntryConfig,
) -> Result<Vec<Arc<Node>>, Whatever> {
    let mut servers = canonicalize_server_nodes(&entry_config.local_servers)?;

    let worker_targets = resolve_entry_worker_targets(entry_config)
        .whatever_context("failed to resolve configured worker users")?;

    for target in &worker_targets {
        servers.extend(discover_worker_servers(target).await?);
    }

    Ok(servers)
}

pub async fn discover_worker_servers(
    target: &ResolvedWorkerTarget,
) -> Result<Vec<Arc<Node>>, Whatever> {
    let genmeta_home = GenmetaHome::new(target.home.join(".genmeta"));
    let identity_names = genmeta_home
        .identities()
        .list()
        .await
        .whatever_context(format!(
            "failed to list identities for worker `{}`",
            target.username
        ))?;

    let mut servers = Vec::new();
    for identity_name in identity_names {
        let conf_path = genmeta_home
            .identities()
            .join_name(identity_name.borrow())
            .join("pishoo.conf");
        if !conf_path.is_file() {
            continue;
        }

        servers.extend(
            load_identity_servers(&conf_path)
                .await
                .whatever_context(format!(
                    "failed to load identity servers from `{}` for worker `{}`",
                    conf_path.display(),
                    target.username
                ))?,
        );
    }

    Ok(servers)
}

pub async fn load_identity_servers(conf_path: &Path) -> Result<Vec<Arc<Node>>, Whatever> {
    let raw = tokio::fs::read(conf_path).await.whatever_context(format!(
        "failed to read identity config `{}`",
        conf_path.display()
    ))?;
    let parsed = gateway::parse::parse(&raw, conf_path.parent()).whatever_context(format!(
        "failed to parse identity config `{}`",
        conf_path.display()
    ))?;
    let Some(Value::Nodes(pishoo_nodes)) = parsed.get("pishoo") else {
        whatever!(
            "identity config `{}` is missing `pishoo` block",
            conf_path.display()
        );
    };
    let Some(pishoo_node) = pishoo_nodes.first() else {
        whatever!(
            "identity config `{}` has empty `pishoo` block",
            conf_path.display()
        );
    };
    let Some(Value::Nodes(server_nodes)) = pishoo_node.get("server") else {
        return Ok(Vec::new());
    };

    canonicalize_server_nodes(server_nodes)
}

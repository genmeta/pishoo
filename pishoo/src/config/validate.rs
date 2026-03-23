use std::{collections::HashSet, sync::Arc};

use gateway::{
    error::Whatever,
    parse::{Node, Value},
};
use snafu::{ResultExt, whatever};

use super::{EntryConfig, ResolvedWorkerTarget, RuntimeShape, resolve_entry_worker_targets};
use crate::{
    config::discover_worker_servers, local_service, naming::canonicalize_server_nodes, policy,
};

#[derive(Debug, Clone)]
pub struct ValidationSummary {
    pub shape: RuntimeShape,
    pub workers: usize,
    pub local_servers: usize,
    pub worker_servers: usize,
}

pub async fn validate_entry_tree(
    entry_config: &EntryConfig,
) -> Result<ValidationSummary, Whatever> {
    if !entry_config.local_servers.is_empty() {
        let _ = policy::load_policy_bundle(entry_config.local_access_rules_uri.as_deref())
            .await
            .whatever_context("failed to validate root-local access rules")?;
        local_service::validate_local_servers(&entry_config.local_servers)
            .await
            .whatever_context("failed to validate root-local servers")?;
    }

    let worker_targets = resolve_entry_worker_targets(entry_config)
        .whatever_context("failed to resolve configured worker users")?;

    let mut seen_server_names = HashSet::new();
    let local_server_nodes = canonicalize_server_nodes(&entry_config.local_servers)?;
    let local_servers =
        register_server_names(&local_server_nodes, &mut seen_server_names, "entry config")?;
    let mut worker_servers = 0usize;
    for target in &worker_targets {
        worker_servers += validate_worker_tree(target, &mut seen_server_names).await?;
    }

    Ok(ValidationSummary {
        shape: entry_config.shape,
        workers: worker_targets.len(),
        local_servers,
        worker_servers,
    })
}

async fn validate_worker_tree(
    target: &ResolvedWorkerTarget,
    seen_server_names: &mut HashSet<String>,
) -> Result<usize, Whatever> {
    let worker_policy_path = target.home.join(".genmeta/pishoo.conf");
    let raw = tokio::fs::read(&worker_policy_path)
        .await
        .whatever_context(format!(
            "failed to read worker policy config for user `{}` at `{}`",
            target.username,
            worker_policy_path.display()
        ))?;
    let parsed =
        gateway::parse::parse(&raw, worker_policy_path.parent()).whatever_context(format!(
            "failed to parse worker policy config for user `{}` at `{}`",
            target.username,
            worker_policy_path.display()
        ))?;
    let Some(Value::Nodes(pishoo_nodes)) = parsed.get("pishoo") else {
        whatever!(
            "worker policy config for user `{}` is missing `pishoo` block",
            target.username
        );
    };
    let Some(policy_root) = pishoo_nodes.first() else {
        whatever!(
            "worker policy config for user `{}` has empty `pishoo` block",
            target.username
        );
    };
    let Some(Value::String(uri)) = policy_root.get("access_rules") else {
        whatever!(
            "worker policy config for user `{}` is missing `access_rules`",
            target.username
        );
    };
    let _ = policy::load_policy_bundle(Some(uri.as_str()))
        .await
        .whatever_context(format!(
            "failed to validate worker policy for user `{}`",
            target.username
        ))?;

    let worker_server_nodes = discover_worker_servers(target).await?;
    local_service::validate_local_servers(&worker_server_nodes)
        .await
        .whatever_context(format!(
            "failed to validate worker identity servers for `{}`",
            target.username
        ))?;
    let worker_server_count = register_server_names(
        &worker_server_nodes,
        seen_server_names,
        &format!("worker `{}`", target.username),
    )?;

    Ok(worker_server_count)
}

fn register_server_names(
    servers: &[Arc<Node>],
    seen_server_names: &mut HashSet<String>,
    scope: &str,
) -> Result<usize, Whatever> {
    let mut count = 0usize;
    for server in servers {
        let Some(Value::ServerName(server_names)) = server.get("server_name") else {
            whatever!("server in {scope} is missing `server_name`");
        };
        for server_name in server_names {
            let normalized = server_name.name.clone();
            if !seen_server_names.insert(normalized.clone()) {
                whatever!("duplicate server_name `{normalized}` found in {scope}");
            }
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nix::unistd::{Gid, Uid};

    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("pishoo crate should live under repo root")
            .to_path_buf()
    }

    fn repo_rules_db_uri() -> String {
        format!(
            "sqlite://{}?mode=rw",
            repo_root().join("rules.db").display()
        )
    }

    fn repo_tls_paths() -> (PathBuf, PathBuf) {
        let base = repo_root().join("keychain/borber.pilot.genmeta.net");
        (
            base.join("borber.pilot.genmeta.net.pem"),
            base.join("borber.pilot.genmeta.net.key"),
        )
    }

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "pishoo-validate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        home
    }

    fn write_worker_layout(home: &PathBuf, server_name: &str) {
        let (cert, key) = repo_tls_paths();
        let genmeta_dir = home.join(".genmeta");
        let identity_dir = genmeta_dir.join("identity/borber.pilot.genmeta.net");
        std::fs::create_dir_all(&identity_dir).expect("create identity dir");
        std::fs::write(
            genmeta_dir.join("pishoo.conf"),
            format!("pishoo {{ access_rules {}; }}", repo_rules_db_uri()),
        )
        .expect("write worker policy");
        std::fs::write(
            identity_dir.join("pishoo.conf"),
            format!(
                "pishoo {{ server {{ listen all 443; server_name {server_name}; ssl_certificate {}; ssl_certificate_key {}; location / {{ root {}; }} }} }}",
                cert.display(),
                key.display(),
                home.display(),
            ),
        )
        .expect("write identity config");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn validate_worker_tree_counts_identity_servers() {
        let home = temp_home();
        write_worker_layout(&home, "borber.pilot.genmeta.net");
        let target = ResolvedWorkerTarget {
            uid: Uid::from_raw(1),
            gid: Gid::from_raw(1),
            username: "tester".to_string(),
            home,
        };
        let mut seen = HashSet::new();

        let count = validate_worker_tree(&target, &mut seen)
            .await
            .expect("worker validation should succeed");

        assert_eq!(count, 1);
        assert!(seen.contains("borber.pilot.genmeta.net"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn validate_worker_tree_rejects_duplicate_server_name() {
        let home = temp_home();
        write_worker_layout(&home, "borber.pilot.genmeta.net");
        let target = ResolvedWorkerTarget {
            uid: Uid::from_raw(1),
            gid: Gid::from_raw(1),
            username: "tester".to_string(),
            home,
        };
        let mut seen = HashSet::from(["borber.pilot.genmeta.net".to_string()]);

        let err = validate_worker_tree(&target, &mut seen)
            .await
            .expect_err("duplicate server name should be rejected");

        assert!(err.to_string().contains("duplicate"));
    }
}

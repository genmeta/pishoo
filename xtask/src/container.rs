//! Shared helpers for running cross-compilation builds inside Docker/Podman
//! containers. Used by both the deb and rpm packaging flows.

use bollard::{
    Docker,
    models::Mount,
    query_parameters::{RemoveContainerOptionsBuilder, StartContainerOptions},
};
use futures_util::StreamExt;
use snafu::{ResultExt, Whatever};

/// Rust toolchain install paths inside the build image (globally readable).
pub(crate) const CARGO_HOME: &str = "/opt/cargo";
pub(crate) const RUSTUP_HOME: &str = "/opt/rustup";

/// Minimum glibc version baked into linux-gnu binaries via cargo-zigbuild.
/// Chosen to match RHEL 8 / Ubuntu 18.04 / Debian 10 / openSUSE Leap 15 baselines.
pub(crate) const ZIG_GLIBC_VERSION: &str = "2.28";

pub(crate) async fn check_docker(docker: &Docker) -> Result<(), Whatever> {
    docker
        .ping()
        .await
        .whatever_context("Docker/Podman daemon not responding")?;
    Ok(())
}

/// Remove a container by name if it exists; ignore "no such container" errors.
///
/// Used to recover from leaked containers left by a previous failed run
/// (e.g. a transient network error during toolchain install) that would
/// otherwise cause a 409 name conflict on the next attempt.
pub(crate) async fn remove_container_if_exists(docker: &Docker, name: &str) {
    let opts = RemoveContainerOptionsBuilder::default().force(true).build();
    match docker.remove_container(name, Some(opts)).await {
        Ok(()) => {
            tracing::info!(
                container = name,
                "removed stale container from previous run"
            );
        }
        Err(e) => {
            let msg = e.to_string();
            if !(msg.contains("No such container") || msg.contains("404")) {
                tracing::debug!(container = name, error = %msg, "ignored non-404 remove error");
            }
        }
    }
}

/// Best-effort container removal used for cleanup-on-exit.
///
/// Logs but does not propagate errors so that it is safe to call on both
/// the success and failure paths without masking the original error.
pub(crate) async fn force_remove_container(docker: &Docker, id: &str) {
    let opts = RemoveContainerOptionsBuilder::default().force(true).build();
    if let Err(e) = docker.remove_container(id, Some(opts)).await {
        tracing::warn!(container = id, error = %e, "failed to remove container on cleanup");
    }
}

/// Start a created container.
pub(crate) async fn start_container(docker: &Docker, container_id: &str) -> Result<(), Whatever> {
    docker
        .start_container(container_id, None::<StartContainerOptions>)
        .await
        .whatever_context("failed to start container")?;
    Ok(())
}

/// Get the uid:gid of the workspace directory on the host.
pub(crate) fn host_uid_gid() -> Result<String, Whatever> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::env::current_dir()
        .and_then(|d| d.metadata())
        .whatever_context("failed to stat workspace directory")?;
    Ok(format!("{}:{}", meta.uid(), meta.gid()))
}

/// Execute a command inside a container and stream output to stderr.
/// When `user` is `Some("uid:gid")`, the command runs as that user.
pub(crate) async fn exec_in_container(
    docker: &Docker,
    container_id: &str,
    cmd: &[&str],
    user: Option<&str>,
) -> Result<(), Whatever> {
    let exec = docker
        .create_exec(
            container_id,
            bollard::models::ExecConfig {
                cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                user: user.map(|u| u.to_string()),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create exec")?;

    let start_result = docker
        .start_exec(&exec.id, None)
        .await
        .whatever_context("failed to start exec")?;

    if let bollard::exec::StartExecResults::Attached { mut output, .. } = start_result {
        while let Some(msg) = output.next().await {
            let msg = msg.whatever_context("exec output error")?;
            eprint!("{msg}");
        }
    }

    let inspect = docker
        .inspect_exec(&exec.id)
        .await
        .whatever_context("failed to inspect exec")?;
    if let Some(code) = inspect.exit_code
        && code != 0
    {
        snafu::whatever!("container command failed with exit code {code}");
    }

    Ok(())
}

/// Bind mounts for the host cargo git/registry cache.
/// This avoids re-downloading crates and allows private git dependencies
/// to work without SSH credentials in the container.
pub(crate) fn cargo_cache_mounts() -> Vec<Mount> {
    use bollard::models::MountTypeEnum;

    let cargo_home = std::env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap_or_default()));
    let mut mounts = Vec::new();
    for subdir in ["git", "registry"] {
        let host_path = format!("{cargo_home}/{subdir}");
        if std::path::Path::new(&host_path).is_dir() {
            mounts.push(Mount {
                target: Some(format!("{CARGO_HOME}/{subdir}")),
                source: Some(host_path),
                typ: Some(MountTypeEnum::BIND),
                ..Default::default()
            });
        }
    }
    mounts
}

/// Resolved sibling bind-mount: canonical host path + basename used as the
/// container target path (`/{basename}`).
#[derive(Clone)]
pub(crate) struct Sibling {
    pub(crate) host: std::path::PathBuf,
    pub(crate) basename: String,
}

pub(crate) fn resolve_siblings(paths: &[std::path::PathBuf]) -> Result<Vec<Sibling>, Whatever> {
    let mut out = Vec::with_capacity(paths.len());
    for raw in paths {
        let host = raw
            .canonicalize()
            .whatever_context(format!("sibling path not found: {}", raw.display()))?;
        if !host.is_dir() {
            snafu::whatever!("sibling path is not a directory: {}", host.display());
        }
        let basename = host
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .ok_or_else(|| {
                snafu::FromString::without_source(format!(
                    "sibling path has no usable basename: {}",
                    host.display()
                ))
            })?;
        out.push(Sibling { host, basename });
    }
    Ok(out)
}

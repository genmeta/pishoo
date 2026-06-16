//! Shared helpers for running cross-compilation builds inside Docker/Podman
//! containers. Used by both the deb and rpm packaging flows.

use std::collections::BTreeMap;

use bollard::{
    Docker,
    models::{Mount, MountType},
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

pub(crate) const DHTTP_BOOTSTRAP_ROOT_CA_TARGET: &str = "/dhttp-bootstrap/root.crt";

const DHTTP_ROOT_CA: &str = "DHTTP_ROOT_CA";
const DHTTP_STUN_SERVER: &str = "DHTTP_STUN_SERVER";
const DHTTP_H3_DNS_SERVER: &str = "DHTTP_H3_DNS_SERVER";
const DHTTP_HTTP_DNS_SERVER: &str = "DHTTP_HTTP_DNS_SERVER";
const DHTTP_MDNS_SERVICE: &str = "DHTTP_MDNS_SERVICE";

const DHTTP_BOOTSTRAP_VARS: [&str; 5] = [
    DHTTP_ROOT_CA,
    DHTTP_STUN_SERVER,
    DHTTP_H3_DNS_SERVER,
    DHTTP_HTTP_DNS_SERVER,
    DHTTP_MDNS_SERVICE,
];

const DHTTP_BOOTSTRAP_SCALAR_VARS: [&str; 4] = [
    DHTTP_STUN_SERVER,
    DHTTP_H3_DNS_SERVER,
    DHTTP_HTTP_DNS_SERVER,
    DHTTP_MDNS_SERVICE,
];

#[derive(Debug)]
pub(crate) struct DhttpBootstrap {
    pub(crate) exports: String,
    pub(crate) mounts: Vec<Mount>,
}

pub(crate) fn dhttp_bootstrap_from_env() -> Result<DhttpBootstrap, Whatever> {
    let mut values = BTreeMap::new();
    for name in DHTTP_BOOTSTRAP_VARS {
        if let Ok(value) = std::env::var(name) {
            values.insert(name.to_string(), value);
        }
    }
    dhttp_bootstrap_from_values(values)
}

pub(crate) fn dhttp_bootstrap_from_values(
    values: BTreeMap<String, String>,
) -> Result<DhttpBootstrap, Whatever> {
    let mut exports = String::new();
    let mut mounts = Vec::new();

    if let Some(host_path) = values.get(DHTTP_ROOT_CA) {
        if host_path.is_empty() {
            snafu::whatever!("{DHTTP_ROOT_CA} must not be empty");
        }
        let host_path = std::path::Path::new(host_path)
            .canonicalize()
            .whatever_context(format!("{DHTTP_ROOT_CA} path not found: {host_path}"))?;
        mounts.push(Mount {
            target: Some(DHTTP_BOOTSTRAP_ROOT_CA_TARGET.to_string()),
            source: Some(host_path.to_string_lossy().into_owned()),
            typ: Some(MountType::BIND),
            read_only: Some(true),
            ..Default::default()
        });
        exports.push_str(&format!(
            "export {DHTTP_ROOT_CA}={DHTTP_BOOTSTRAP_ROOT_CA_TARGET}\n"
        ));
    }

    for name in DHTTP_BOOTSTRAP_SCALAR_VARS {
        if let Some(value) = values.get(name) {
            if value.is_empty() {
                snafu::whatever!("{name} must not be empty");
            }
            exports.push_str(&format!("export {name}={}\n", shell_single_quote(value)));
        }
    }

    Ok(DhttpBootstrap { exports, mounts })
}

fn shell_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

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

pub(crate) async fn install_cargo_config(
    docker: &Docker,
    container_id: &str,
    config: Option<&str>,
) -> Result<(), Whatever> {
    let Some(config) = config else {
        return Ok(());
    };

    let script = format!(
        r#"set -e
mkdir -p {CARGO_HOME}
cat > {CARGO_HOME}/config.toml <<'GENMETA_XTASK_CARGO_CONFIG'
{config}GENMETA_XTASK_CARGO_CONFIG
"#
    );
    exec_in_container(docker, container_id, &["bash", "-c", &script], None)
        .await
        .whatever_context("failed to install cargo config")?;
    Ok(())
}

/// Bind mounts for the host cargo git/registry cache.
/// This avoids re-downloading crates and allows private git dependencies
/// to work without SSH credentials in the container.
pub(crate) fn cargo_cache_mounts() -> Vec<Mount> {
    use bollard::models::MountType;

    let cargo_home = std::env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap_or_default()));
    let mut mounts = Vec::new();
    for subdir in ["git", "registry"] {
        let host_path = format!("{cargo_home}/{subdir}");
        if std::path::Path::new(&host_path).is_dir() {
            mounts.push(Mount {
                target: Some(format!("{CARGO_HOME}/{subdir}")),
                source: Some(host_path),
                typ: Some(MountType::BIND),
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

pub(crate) fn cargo_config_from_siblings(siblings: &[Sibling]) -> Option<String> {
    let mut config = String::new();
    let mut registry_patches = Vec::new();

    if has_sibling(siblings, "dhttp") {
        let packages = [
            ("dhttp", "/dhttp/dhttp"),
            ("dhttp-access", "/dhttp/access"),
            ("dhttp-home", "/dhttp/home"),
            ("dhttp-identity", "/dhttp/identity"),
        ];
        registry_patches.extend(packages);
        push_patch_section(
            &mut config,
            "https://github.com/genmeta/dhttp.git",
            &packages,
        );
    }

    if has_sibling(siblings, "ddns") {
        let packages = [("dyns", "/ddns")];
        registry_patches.extend(packages);
        push_patch_section(
            &mut config,
            "https://github.com/genmeta/ddns.git",
            &packages,
        );
    }

    if has_sibling(siblings, "h3x") {
        let packages = [("h3x", "/h3x")];
        registry_patches.extend(packages);
        push_patch_section(&mut config, "https://github.com/genmeta/h3x.git", &packages);
    }

    if has_sibling(siblings, "dssh") {
        let packages = [("dshell", "/dssh")];
        registry_patches.extend(packages);
        push_patch_section(
            &mut config,
            "https://github.com/genmeta/dssh.git",
            &packages,
        );
    }

    if has_sibling(siblings, "dquic") {
        let packages = [("dquic", "/dquic/dquic")];
        registry_patches.extend(packages);
        push_patch_section(
            &mut config,
            "https://github.com/genmeta/dquic.git",
            &packages,
        );
    }

    if has_sibling(siblings, "rankey") {
        let packages = [("rankey", "/rankey")];
        registry_patches.extend(packages);
        push_patch_section(
            &mut config,
            "https://github.com/genmeta/rankey.git",
            &packages,
        );
    }

    if registry_patches.is_empty() {
        None
    } else {
        let mut registry_config = String::new();
        push_registry_patch_section(&mut registry_config, &registry_patches);
        registry_config.push_str(&config);
        Some(registry_config)
    }
}

fn has_sibling(siblings: &[Sibling], basename: &str) -> bool {
    siblings.iter().any(|sibling| sibling.basename == basename)
}

fn push_registry_patch_section(config: &mut String, packages: &[(&str, &str)]) {
    config.push_str("[patch.crates-io]\n");
    for (package, path) in packages {
        config.push_str(package);
        config.push_str(" = { path = \"");
        config.push_str(path);
        config.push_str("\" }\n");
    }
    config.push('\n');
}

fn push_patch_section(config: &mut String, source: &str, packages: &[(&str, &str)]) {
    config.push_str("[patch.\"");
    config.push_str(source);
    config.push_str("\"]\n");
    for (package, path) in packages {
        config.push_str(package);
        config.push_str(" = { path = \"");
        config.push_str(path);
        config.push_str("\" }\n");
    }
    config.push('\n');
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bollard::models::MountType;

    use super::*;

    #[test]
    fn dhttp_bootstrap_exports_scalars_and_mounts_root_ca() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let root_ca = tempdir.path().join("root.crt");
        std::fs::write(&root_ca, "test root ca").expect("write root ca");

        let mut values = BTreeMap::new();
        values.insert(
            "DHTTP_ROOT_CA".to_string(),
            root_ca.to_string_lossy().into_owned(),
        );
        values.insert(
            "DHTTP_STUN_SERVER".to_string(),
            "nat.genmeta.net:20004".to_string(),
        );
        values.insert(
            "DHTTP_H3_DNS_SERVER".to_string(),
            "https://dns.genmeta.net:4433".to_string(),
        );
        values.insert(
            "DHTTP_HTTP_DNS_SERVER".to_string(),
            "https://dns.genmeta.net".to_string(),
        );
        values.insert(
            "DHTTP_MDNS_SERVICE".to_string(),
            "_genmeta.local".to_string(),
        );

        let bootstrap = dhttp_bootstrap_from_values(values).expect("build bootstrap");

        assert_eq!(bootstrap.mounts.len(), 1);
        let mount = &bootstrap.mounts[0];
        assert_eq!(
            mount.target.as_deref(),
            Some(DHTTP_BOOTSTRAP_ROOT_CA_TARGET)
        );
        assert_eq!(
            mount.source.as_deref(),
            Some(
                root_ca
                    .canonicalize()
                    .expect("canonicalize root ca")
                    .to_str()
                    .expect("utf-8 path")
            )
        );
        assert_eq!(mount.typ, Some(MountType::BIND));
        assert_eq!(mount.read_only, Some(true));

        assert!(
            bootstrap
                .exports
                .contains("export DHTTP_ROOT_CA=/dhttp-bootstrap/root.crt\n")
        );
        assert!(!bootstrap.exports.contains("export ROOT_CA="));
        assert!(
            bootstrap
                .exports
                .contains("export DHTTP_STUN_SERVER='nat.genmeta.net:20004'\n")
        );
        assert!(
            bootstrap
                .exports
                .contains("export DHTTP_H3_DNS_SERVER='https://dns.genmeta.net:4433'\n")
        );
        assert!(
            bootstrap
                .exports
                .contains("export DHTTP_HTTP_DNS_SERVER='https://dns.genmeta.net'\n")
        );
        assert!(
            bootstrap
                .exports
                .contains("export DHTTP_MDNS_SERVICE='_genmeta.local'\n")
        );
    }

    #[test]
    fn dhttp_bootstrap_allows_missing_values() {
        let bootstrap =
            dhttp_bootstrap_from_values(BTreeMap::new()).expect("missing values are allowed");

        assert!(bootstrap.exports.is_empty());
        assert!(bootstrap.mounts.is_empty());
    }

    #[test]
    fn dhttp_bootstrap_rejects_empty_root_ca() {
        let mut values = BTreeMap::new();
        values.insert("DHTTP_ROOT_CA".to_string(), String::new());

        let error = dhttp_bootstrap_from_values(values).expect_err("empty value must fail");

        assert_eq!(error.to_string(), "DHTTP_ROOT_CA must not be empty");
    }

    #[test]
    fn dhttp_bootstrap_escapes_single_quotes_in_scalar_values() {
        let mut values = BTreeMap::new();
        values.insert(
            "DHTTP_STUN_SERVER".to_string(),
            "nat'genmeta.net:20004".to_string(),
        );

        let bootstrap = dhttp_bootstrap_from_values(values).expect("build bootstrap");

        assert_eq!(
            bootstrap.exports,
            "export DHTTP_STUN_SERVER='nat'\\''genmeta.net:20004'\n"
        );
        assert!(bootstrap.mounts.is_empty());
    }

    #[test]
    fn cargo_config_from_siblings_patches_genmeta_git_and_registry_sources() {
        let siblings = vec![
            Sibling {
                host: "/host/reimu/dhttp".into(),
                basename: "dhttp".to_string(),
            },
            Sibling {
                host: "/host/reimu/ddns".into(),
                basename: "ddns".to_string(),
            },
            Sibling {
                host: "/host/reimu/dquic".into(),
                basename: "dquic".to_string(),
            },
            Sibling {
                host: "/host/reimu/rankey".into(),
                basename: "rankey".to_string(),
            },
        ];

        let config = cargo_config_from_siblings(&siblings).expect("config should be rendered");
        assert!(config.contains("[patch.crates-io]"));
        assert!(config.contains("dhttp = { path = \"/dhttp/dhttp\" }"));
        assert!(config.contains("dhttp-identity = { path = \"/dhttp/identity\" }"));
        assert!(config.contains("dyns = { path = \"/ddns\" }"));
        assert!(config.contains("dquic = { path = \"/dquic/dquic\" }"));
        assert!(config.contains("rankey = { path = \"/rankey\" }"));
        assert!(config.contains("[patch.\"https://github.com/genmeta/dhttp.git\"]"));
        assert!(config.contains("dhttp = { path = \"/dhttp/dhttp\" }"));
        assert!(config.contains("dhttp-identity = { path = \"/dhttp/identity\" }"));
        assert!(config.contains("[patch.\"https://github.com/genmeta/ddns.git\"]"));
        assert!(config.contains("dyns = { path = \"/ddns\" }"));
        assert!(config.contains("[patch.\"https://github.com/genmeta/dquic.git\"]"));
        assert!(config.contains("dquic = { path = \"/dquic/dquic\" }"));
        assert!(config.contains("[patch.\"https://github.com/genmeta/rankey.git\"]"));
        assert!(config.contains("rankey = { path = \"/rankey\" }"));
        assert!(!config.contains("ddns ="));
    }

    #[test]
    fn cargo_config_from_siblings_returns_none_without_supported_siblings() {
        let siblings = vec![Sibling {
            host: "/host/reimu/other".into(),
            basename: "other".to_string(),
        }];

        assert!(cargo_config_from_siblings(&siblings).is_none());
    }

    #[test]
    fn deb_rules_filter_unsupported_aarch64_zig_linker_arg() {
        let rules = include_str!("../deb/rules");

        assert!(rules.contains("CARGO_ZIGBUILD_ZIG_PATH"));
        assert!(rules.contains("-Wl,--fix-cortex-a53-843419|--fix-cortex-a53-843419"));
        assert!(rules.contains("-Clinker-flavor=gnu-lld-cc"));
    }
}

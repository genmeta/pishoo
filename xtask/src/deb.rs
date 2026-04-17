use bollard::{
    Docker,
    models::{ContainerConfig, ContainerCreateBody, HostConfig, Mount, MountTypeEnum},
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
        RemoveContainerOptionsBuilder, StartContainerOptions,
    },
};
use futures_util::StreamExt;
use snafu::{ResultExt, Whatever};
use tracing::{Instrument, info, info_span};

use crate::{DebTarget, Feature, package_version, target_dir};

/// Toolchain install paths inside the Docker image (globally readable).
const CARGO_HOME: &str = "/opt/cargo";
const RUSTUP_HOME: &str = "/opt/rustup";

const CARGO_NAME: &str = "pishoo";

/// Base Docker image for cross-compilation.
const BASE_IMAGE: &str = "debian:bookworm";

/// Image tag prefix for pishoo deb builds.
const IMAGE_TAG_PREFIX: &str = "pishoo-deb-v2";

/// Relative path from workspace root to the debian packaging source files.
const DEBIAN_PKG_DIR: &str = "xtask/deb";

fn deb_arch(triple: &str) -> Result<&'static str, Whatever> {
    match triple {
        "x86_64-unknown-linux-gnu" => Ok("amd64"),
        "aarch64-unknown-linux-gnu" => Ok("arm64"),
        "armv7-unknown-linux-gnueabihf" => Ok("armhf"),
        "i686-unknown-linux-gnu" => Ok("i386"),
        _ => snafu::whatever!("unsupported deb target triple: {triple}"),
    }
}

/// GNU architecture prefix used for cross-compilation lib paths.
fn gnu_arch(triple: &str) -> Result<&'static str, Whatever> {
    match triple {
        "x86_64-unknown-linux-gnu" => Ok("x86_64-linux-gnu"),
        "aarch64-unknown-linux-gnu" => Ok("aarch64-linux-gnu"),
        "armv7-unknown-linux-gnueabihf" => Ok("arm-linux-gnueabihf"),
        "i686-unknown-linux-gnu" => Ok("i386-linux-gnu"),
        _ => snafu::whatever!("unsupported gnu arch for triple: {triple}"),
    }
}

async fn check_docker(docker: &Docker) -> Result<(), Whatever> {
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
async fn remove_container_if_exists(docker: &Docker, name: &str) {
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
async fn force_remove_container(docker: &Docker, id: &str) {
    let opts = RemoveContainerOptionsBuilder::default().force(true).build();
    if let Err(e) = docker.remove_container(id, Some(opts)).await {
        tracing::warn!(container = id, error = %e, "failed to remove container on cleanup");
    }
}

/// Ensure the build image exists for the given target triple.
/// Installs cross-compilation toolchain, libc-dev, and libpam0g-dev.
async fn ensure_image(docker: &Docker, triple: &str) -> Result<String, Whatever> {
    let deb = deb_arch(triple)?;
    let tag = format!("xtask-{triple}:{IMAGE_TAG_PREFIX}");

    if docker.inspect_image(&tag).await.is_ok() {
        info!(tag, "image already exists");
        return Ok(tag);
    }

    info!(tag, "building image");

    // Ensure base image exists
    let mut pull_stream = docker.create_image(
        Some(
            CreateImageOptionsBuilder::default()
                .from_image(BASE_IMAGE)
                .build(),
        ),
        None,
        None,
    );
    while let Some(result) = pull_stream.next().await {
        result.whatever_context(format!("failed to pull base image {BASE_IMAGE}"))?;
    }

    // Create temp container from base
    let container_name = format!("{CARGO_NAME}-xtask-setup-{triple}");
    // Remove a leaked container left by a previous failed run so we can retry.
    remove_container_if_exists(docker, &container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(BASE_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create setup container")?;
    let container_id = container.id.clone();

    // Any failure past this point must still remove the container, otherwise
    // the name collides on retry.
    let setup_result = ensure_image_inner(docker, &container_id, triple, deb).await;

    if setup_result.is_err() {
        force_remove_container(docker, &container_id).await;
        setup_result?;
        unreachable!();
    }

    // Commit
    let repo = tag.split(':').next().unwrap_or(&tag);
    let img_tag = tag.split(':').nth(1).unwrap_or(IMAGE_TAG_PREFIX);
    let commit_result = docker
        .commit_container(
            CommitContainerOptionsBuilder::default()
                .container(&container_id)
                .repo(repo)
                .tag(img_tag)
                .build(),
            ContainerConfig::default(),
        )
        .await
        .whatever_context("failed to commit image");

    // Always remove the setup container before returning.
    force_remove_container(docker, &container_id).await;
    commit_result?;

    info!(tag, "image ready");
    Ok(tag)
}

/// Run the toolchain-installation steps inside an already-created container.
async fn ensure_image_inner(
    docker: &Docker,
    container_id: &str,
    triple: &str,
    deb: &str,
) -> Result<(), Whatever> {
    docker
        .start_container(container_id, None::<StartContainerOptions>)
        .await
        .whatever_context("failed to start setup container")?;

    // Install Rust toolchain, Zig, cargo-zigbuild, and cross-compilation libs.
    // Toolchain is installed to /opt/cargo + /opt/rustup so any uid can use it.
    let setup_script = format!(
        r#"set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install --assume-yes -qq \
    ca-certificates curl build-essential pkg-config libclang-dev wget

# install rust into globally readable paths
export CARGO_HOME={CARGO_HOME}
export RUSTUP_HOME={RUSTUP_HOME}
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- --default-toolchain nightly --profile minimal -y
export PATH="{CARGO_HOME}/bin:$PATH"
rustup target add {triple}

# install zig
wget -q https://ziglang.org/download/0.14.0/zig-linux-x86_64-0.14.0.tar.xz
tar -xf zig-linux-x86_64-0.14.0.tar.xz
mv zig-linux-x86_64-0.14.0 /usr/local/zig
ln -s /usr/local/zig/zig /usr/local/bin/zig
rm zig-linux-x86_64-0.14.0.tar.xz

cargo install cargo-zigbuild

# cross-compilation libraries
dpkg --add-architecture {deb}
apt-get update -qq
apt-get install --assume-yes -qq libc-dev:{deb} libpam0g-dev:{deb} dpkg-dev debhelper fakeroot

# make toolchain readable by any user
chmod -R a+rX {CARGO_HOME} {RUSTUP_HOME}
"#
    );
    exec_in_container(docker, container_id, &["bash", "-c", &setup_script], None).await?;
    Ok(())
}

/// Get the uid:gid of the workspace directory on the host.
fn host_uid_gid() -> Result<String, Whatever> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::env::current_dir()
        .and_then(|d| d.metadata())
        .whatever_context("failed to stat workspace directory")?;
    Ok(format!("{}:{}", meta.uid(), meta.gid()))
}

/// Execute a command inside a container and stream output to stderr.
/// When `user` is `Some("uid:gid")`, the command runs as that user.
async fn exec_in_container(
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
fn cargo_cache_mounts() -> Vec<Mount> {
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

/// Build the arch-independent pishoo-common config package.
async fn run_common(docker: &Docker, version: &str) -> Result<(), Whatever> {
    info!("starting common deb package build");
    let target_dir = target_dir()?;
    let out_dir = target_dir.join("common").join("deb");
    tokio::fs::create_dir_all(&out_dir)
        .await
        .whatever_context(format!("failed to create {}", out_dir.display()))?;

    let workspace_dir =
        std::env::current_dir().whatever_context("failed to get current directory")?;

    // Ensure base image exists (pishoo-common only needs debhelper + dpkg-deb)
    let mut pull_stream = docker.create_image(
        Some(
            CreateImageOptionsBuilder::default()
                .from_image(BASE_IMAGE)
                .build(),
        ),
        None,
        None,
    );
    while let Some(result) = pull_stream.next().await {
        result.whatever_context(format!("failed to pull base image {BASE_IMAGE}"))?;
    }

    let container_name = format!("{CARGO_NAME}-xtask-deb-common");
    remove_container_if_exists(docker, &container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(BASE_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                host_config: Some(HostConfig {
                    mounts: Some(vec![Mount {
                        target: Some("/workspace".into()),
                        source: Some(workspace_dir.to_string_lossy().into_owned()),
                        typ: Some(MountTypeEnum::BIND),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create common container")?;
    let container_id = container.id.clone();

    // Always remove the common container before returning, success or not.
    let result = run_common_inner(docker, &container_id, version).await;
    force_remove_container(docker, &container_id).await;
    result?;

    let deb_name = format!("pishoo-common_{version}-1_all.deb");
    info!(deb_name, "produced");
    info!("finished common deb package build");
    Ok(())
}

async fn run_common_inner(
    docker: &Docker,
    container_id: &str,
    version: &str,
) -> Result<(), Whatever> {
    docker
        .start_container(container_id, None::<StartContainerOptions>)
        .await
        .whatever_context("failed to start common container")?;

    // Install debhelper inside the base container (no cross-compilation image needed)
    let setup_script = r#"set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install --assume-yes -qq debhelper fakeroot
"#;
    exec_in_container(docker, container_id, &["bash", "-c", setup_script], None).await?;

    // Prepare debian source tree under target/common/deb/src/ and run dpkg-buildpackage.
    // Products (.deb etc.) land in target/common/deb/ (one level above src/).
    // Runs as host uid:gid so files in target/ are owned by the host user.
    let user = host_uid_gid()?;
    let build_script = format!(
        r#"set -e
SRC=/workspace/target/common/deb/src
mkdir -p "$SRC/debian"
cp -r /workspace/{DEBIAN_PKG_DIR}/. "$SRC/debian/"
printf '{CARGO_NAME} ({version}-1) unstable; urgency=low\n\n  * release {version}\n\n -- Genmeta Tech Limited <support@genmeta.net>  %s\n' \
    "$(date -R)" > "$SRC/debian/changelog"
cd "$SRC"
dpkg-buildpackage -A -uc -us -d
"#
    );
    exec_in_container(
        docker,
        container_id,
        &["bash", "-c", &build_script],
        Some(&user),
    )
    .await?;

    Ok(())
}

pub async fn run(targets: &[DebTarget], features: &[Feature]) -> Result<(), Whatever> {
    info!(target_count = targets.len(), "starting deb dist build");
    let docker = Docker::connect_with_local_defaults()
        .whatever_context("failed to connect to Docker/Podman")?;
    check_docker(&docker).await?;

    let version = package_version(CARGO_NAME)?;
    let target_dir = target_dir()?;

    let mut tasks = tokio::task::JoinSet::new();

    for &target in targets {
        if matches!(target, DebTarget::Common) {
            let docker = docker.clone();
            let version = version.clone();
            info!("queued common deb package build");
            tasks.spawn(
                async move { run_common(&docker, &version).await }
                    .instrument(info_span!("deb", triple = "common")),
            );
            continue;
        }

        let docker = docker.clone();
        let triple = target.triple();
        info!(triple, "queued deb target build");
        let span = info_span!("deb", triple);
        let version = version.clone();
        let target_dir = target_dir.clone();
        let features = features.to_vec();
        tasks.spawn(
            async move { build_one(&docker, triple, &version, &target_dir, &features).await }
                .instrument(span),
        );
    }

    while let Some(result) = tasks.join_next().await {
        result.whatever_context("deb build task panicked")??;
    }

    info!("finished deb dist build");

    Ok(())
}

async fn build_one(
    docker: &Docker,
    triple: &str,
    version: &str,
    target_dir: &std::path::Path,
    features: &[Feature],
) -> Result<(), Whatever> {
    let has_sshd = features
        .iter()
        .any(|f| matches!(f, Feature::Sshd | Feature::Pam));
    let arch = deb_arch(triple)?;
    let gnu = gnu_arch(triple)?;
    info!(triple, "ensuring build image");
    let image = ensure_image(docker, triple).await?;

    let deb_name = format!("{CARGO_NAME}_{version}-1_{arch}.deb");
    let out_dir = target_dir.join(triple).join("release").join("deb");
    tokio::fs::create_dir_all(&out_dir)
        .await
        .whatever_context(format!("failed to create {}", out_dir.display()))?;

    let workspace_dir =
        std::env::current_dir().whatever_context("failed to get current directory")?;

    let mut mounts = vec![Mount {
        target: Some("/workspace".into()),
        source: Some(workspace_dir.to_string_lossy().into_owned()),
        typ: Some(MountTypeEnum::BIND),
        ..Default::default()
    }];

    mounts.extend(cargo_cache_mounts());

    // Forward ROOT_CA into the container if set on the host.
    let root_ca_env = if let Ok(host_path) = std::env::var("ROOT_CA") {
        let host_path = std::path::Path::new(&host_path)
            .canonicalize()
            .whatever_context(format!("ROOT_CA path not found: {host_path}"))?;
        mounts.push(Mount {
            target: Some("/root-ca/root.crt".into()),
            source: Some(host_path.to_string_lossy().into_owned()),
            typ: Some(MountTypeEnum::BIND),
            read_only: Some(true),
            ..Default::default()
        });
        "export ROOT_CA=/root-ca/root.crt && "
    } else {
        ""
    };

    let container_name = format!("{CARGO_NAME}-xtask-deb-{triple}");
    info!(triple, container = %container_name, "creating build container");
    remove_container_if_exists(docker, &container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(image.clone()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                host_config: Some(HostConfig {
                    mounts: Some(mounts),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create build container")?;
    let container_id = container.id.clone();

    // Build cargo features string
    let cargo_features = {
        let names: Vec<&str> = features
            .iter()
            .map(|f| match f {
                Feature::Sshd => "sshd",
                Feature::Pam => "pam",
            })
            .collect();
        if names.is_empty() {
            String::new()
        } else {
            names.join(",")
        }
    };

    // Environment variables for debian/rules override_dh_auto_build.
    // pishoo-worker is always required; pishoo-ssh-session only when sshd feature is enabled.
    let worker_env = "export PISHOO_WORKER_BIN=/usr/lib/pishoo/pishoo-worker && ";
    let ssh_session_env = if has_sshd {
        "export PISHOO_SSH_SESSION_BIN=/usr/lib/pishoo/pishoo-ssh-session && "
    } else {
        ""
    };

    let cargo_features_env = if cargo_features.is_empty() {
        String::new()
    } else {
        format!("export CARGO_FEATURES={cargo_features} && ")
    };

    // Run the actual build; always clean up the container regardless of outcome.
    let result = build_one_inner(
        docker,
        &container_id,
        triple,
        version,
        arch,
        gnu,
        root_ca_env,
        worker_env,
        ssh_session_env,
        &cargo_features_env,
    )
    .await;
    force_remove_container(docker, &container_id).await;
    result?;

    info!(deb_name, "produced");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn build_one_inner(
    docker: &Docker,
    container_id: &str,
    triple: &str,
    version: &str,
    arch: &str,
    gnu: &str,
    root_ca_env: &str,
    worker_env: &str,
    ssh_session_env: &str,
    cargo_features_env: &str,
) -> Result<(), Whatever> {
    docker
        .start_container(container_id, None::<StartContainerOptions>)
        .await
        .whatever_context("failed to start build container")?;
    info!(triple, "build container started");

    // Install cross-compilation binutils (needs root).
    let install_binutils = format!("apt-get install -y -qq binutils-{gnu} 2>/dev/null || true");
    exec_in_container(
        docker,
        container_id,
        &["bash", "-c", &install_binutils],
        None,
    )
    .await?;

    // dpkg-buildpackage -B builds only Architecture: any packages (pishoo binary).
    // -a{arch} sets the host architecture for cross-compilation.
    // Prepare debian source tree under target/{triple}/release/deb/src/ so that
    // all temp files and products stay inside target/ (bind-mounted, gitignored).
    // Runs as host uid:gid so files in target/ are owned by the host user.
    let user = host_uid_gid()?;
    let build_script = format!(
        r#"set -e
export HOME=/tmp
export PATH="{CARGO_HOME}/bin:/usr/local/zig:$PATH"
export RUSTUP_HOME={RUSTUP_HOME}
export CARGO_HOME={CARGO_HOME}
export TRIPLE={triple}
export DEB_HOST_MULTIARCH={gnu}
{root_ca_env}{worker_env}{ssh_session_env}{cargo_features_env}
SRC=/workspace/target/{triple}/release/deb/src
mkdir -p "$SRC/debian"
cp -r /workspace/{DEBIAN_PKG_DIR}/. "$SRC/debian/"
printf '{CARGO_NAME} ({version}-1) unstable; urgency=low\n\n  * release {version}\n\n -- Genmeta Tech Limited <support@genmeta.net>  %s\n' \
    "$(date -R)" > "$SRC/debian/changelog"
cd "$SRC"
dpkg-buildpackage -B -uc -us -d -a{arch}
"#
    );

    info!(triple, "starting dpkg-buildpackage inside container");
    exec_in_container(
        docker,
        container_id,
        &["bash", "-c", &build_script],
        Some(&user),
    )
    .await?;
    info!(triple, "dpkg-buildpackage finished inside container");

    Ok(())
}

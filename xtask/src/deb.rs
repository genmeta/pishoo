use std::path::Path;

use bollard::{
    Docker,
    models::{ContainerConfig, ContainerCreateBody, HostConfig, Mount, MountTypeEnum},
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
        DownloadFromContainerOptionsBuilder, RemoveContainerOptionsBuilder, StartContainerOptions,
    },
};
use futures_util::StreamExt;
use snafu::{ResultExt, Whatever};
use tracing::{Instrument, info, info_span};

use crate::{package_version, target_dir};

const CARGO_NAME: &str = "pishoo";

/// Base Docker image for cross-compilation.
const BASE_IMAGE: &str = "debian:bookworm";

/// Image tag prefix for pishoo deb builds.
const IMAGE_TAG_PREFIX: &str = "pishoo-deb";

/// pishoo-common version (arch-independent config package).
const COMMON_VERSION: &str = "0.2.1";

fn deb_arch(triple: &str) -> Result<&'static str, Whatever> {
    match triple {
        "x86_64-unknown-linux-gnu" => Ok("amd64"),
        "aarch64-unknown-linux-gnu" => Ok("arm64"),
        "armv7-unknown-linux-gnueabihf" => Ok("armhf"),
        _ => snafu::whatever!("unsupported deb target triple: {triple}"),
    }
}

/// GNU architecture prefix used for cross-compilation lib paths.
fn gnu_arch(triple: &str) -> Result<&'static str, Whatever> {
    match triple {
        "x86_64-unknown-linux-gnu" => Ok("x86_64-linux-gnu"),
        "aarch64-unknown-linux-gnu" => Ok("aarch64-linux-gnu"),
        "armv7-unknown-linux-gnueabihf" => Ok("arm-linux-gnueabihf"),
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
    let container_name = format!("xtask-setup-{triple}");
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

    docker
        .start_container(&container.id, None::<StartContainerOptions>)
        .await
        .whatever_context("failed to start setup container")?;

    // Install Rust toolchain, Zig, cargo-zigbuild, and cross-compilation libs
    let setup_script = format!(
        r#"set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install --assume-yes -qq \
    ca-certificates curl build-essential pkg-config libclang-dev wget

# install rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- --default-toolchain nightly --profile minimal -y
source /root/.cargo/env
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
apt-get install --assume-yes -qq libc-dev:{deb} libpam0g-dev:{deb} dpkg-dev
"#
    );
    exec_in_container(docker, &container.id, &["bash", "-c", &setup_script]).await?;

    // Commit
    let repo = tag.split(':').next().unwrap_or(&tag);
    let img_tag = tag.split(':').nth(1).unwrap_or(IMAGE_TAG_PREFIX);
    docker
        .commit_container(
            CommitContainerOptionsBuilder::default()
                .container(&container.id)
                .repo(repo)
                .tag(img_tag)
                .build(),
            ContainerConfig::default(),
        )
        .await
        .whatever_context("failed to commit image")?;

    // Cleanup
    docker
        .remove_container(
            &container.id,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .whatever_context("failed to remove setup container")?;

    info!(tag, "image ready");
    Ok(tag)
}

/// Execute a command inside a container and stream output to stderr.
async fn exec_in_container(
    docker: &Docker,
    container_id: &str,
    cmd: &[&str],
) -> Result<(), Whatever> {
    let exec = docker
        .create_exec(
            container_id,
            bollard::models::ExecConfig {
                cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
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

/// Copy a file from a container to the local filesystem.
async fn copy_from_container(
    docker: &Docker,
    container_id: &str,
    container_path: &str,
    local_dir: &Path,
) -> Result<(), Whatever> {
    let mut tar_stream = docker.download_from_container(
        container_id,
        Some(
            DownloadFromContainerOptionsBuilder::default()
                .path(container_path)
                .build(),
        ),
    );

    let mut tar_data = Vec::new();
    while let Some(chunk) = tar_stream.next().await {
        let chunk = chunk.whatever_context("failed to download from container")?;
        tar_data.extend_from_slice(&chunk);
    }

    let mut archive = tar::Archive::new(&tar_data[..]);
    std::fs::create_dir_all(local_dir)
        .whatever_context(format!("failed to create {}", local_dir.display()))?;

    for entry in archive
        .entries()
        .whatever_context("failed to read tar entries")?
    {
        let mut entry = entry.whatever_context("failed to read tar entry")?;
        let path = entry
            .path()
            .whatever_context("failed to read entry path")?
            .into_owned();
        let filename = path.file_name().unwrap_or(path.as_os_str());
        let dest = local_dir.join(filename);
        let mut file = std::fs::File::create(&dest)
            .whatever_context(format!("failed to create {}", dest.display()))?;
        std::io::copy(&mut entry, &mut file)
            .whatever_context(format!("failed to write {}", dest.display()))?;
    }

    Ok(())
}

fn format_control(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Bind mounts for the host cargo git/registry cache (read-only).
fn cargo_cache_mounts() -> Vec<Mount> {
    let cargo_home = std::env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap_or_default()));
    let mut mounts = Vec::new();
    for subdir in ["git", "registry"] {
        let host_path = format!("{cargo_home}/{subdir}");
        if std::path::Path::new(&host_path).is_dir() {
            mounts.push(Mount {
                target: Some(format!("/root/.cargo/{subdir}")),
                source: Some(host_path),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            });
        }
    }
    mounts
}

/// Build the arch-independent pishoo-common config package.
async fn run_common(docker: &Docker) -> Result<(), Whatever> {
    let target_dir = target_dir()?;
    let deb_name = format!("pishoo-common_{COMMON_VERSION}-1_all.deb");
    let out_dir = target_dir.join("common").join("deb");
    std::fs::create_dir_all(&out_dir)
        .whatever_context(format!("failed to create {}", out_dir.display()))?;

    let workspace_dir =
        std::env::current_dir().whatever_context("failed to get current directory")?;

    // Use any base image — pishoo-common just needs dpkg-deb
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

    let container_name = "xtask-deb-common";
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(BASE_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                working_dir: Some("/workspace".into()),
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

    docker
        .start_container(&container.id, None::<StartContainerOptions>)
        .await
        .whatever_context("failed to start common container")?;

    let script = format!(
        r#"set -e
mkdir -p /staging/DEBIAN
cp -r /workspace/pishoo/pkg/common/* /staging/
cp -r /workspace/pishoo/pkg/debian/* /staging/DEBIAN/
sed -i 's/Version:.*/Version: {COMMON_VERSION}-1/' /staging/DEBIAN/control
mkdir -p /output
dpkg-deb -b /staging /output/{deb_name}
"#
    );
    exec_in_container(docker, &container.id, &["bash", "-c", &script]).await?;

    copy_from_container(
        docker,
        &container.id,
        &format!("/output/{deb_name}"),
        &out_dir,
    )
    .await?;

    docker
        .remove_container(
            &container.id,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .whatever_context("failed to remove common container")?;

    info!(deb_name, "produced");
    Ok(())
}

pub async fn run(targets: &[String]) -> Result<(), Whatever> {
    let docker = Docker::connect_with_local_defaults()
        .whatever_context("failed to connect to Docker/Podman")?;
    check_docker(&docker).await?;

    let version = package_version(CARGO_NAME)?;
    let target_dir = target_dir()?;

    let mut tasks = tokio::task::JoinSet::new();

    for triple in targets {
        if triple == "common" {
            let docker = docker.clone();
            tasks.spawn(
                async move { run_common(&docker).await.map_err(|e| e.to_string()) }
                    .instrument(info_span!("deb", triple = "common")),
            );
            continue;
        }

        let docker = docker.clone();
        let span = info_span!("deb", %triple);
        let triple = triple.clone();
        let version = version.clone();
        let target_dir = target_dir.clone();
        tasks.spawn(
            async move {
                build_one(&docker, &triple, &version, &target_dir)
                    .await
                    .map_err(|e| e.to_string())
            }
            .instrument(span),
        );
    }

    while let Some(result) = tasks.join_next().await {
        let inner = result.whatever_context("deb build task panicked")?;
        if let Err(msg) = inner {
            snafu::whatever!("{msg}");
        }
    }

    Ok(())
}

async fn build_one(
    docker: &Docker,
    triple: &str,
    version: &str,
    target_dir: &std::path::Path,
) -> Result<(), Whatever> {
    let arch = deb_arch(triple)?;
    let gnu = gnu_arch(triple)?;
    let image = ensure_image(docker, triple).await?;

    let deb_name = format!("{CARGO_NAME}_{version}-1_{arch}.deb");
    let out_dir = target_dir.join(triple).join("release").join("deb");
    std::fs::create_dir_all(&out_dir)
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

    let container_name = format!("xtask-deb-{triple}");
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
                working_dir: Some("/workspace".into()),
                host_config: Some(HostConfig {
                    mounts: Some(mounts),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create build container")?;

    docker
        .start_container(&container.id, None::<StartContainerOptions>)
        .await
        .whatever_context("failed to start build container")?;

    // Build with sshd feature + env vars for binary discovery
    let build_cmd = format!(
        "source /root/.cargo/env && \
         export RUSTFLAGS=\"${{RUSTFLAGS:-}} -L /usr/lib/{gnu}\" && \
         export PISHOO_WORKER_BIN=/usr/lib/pishoo/pishoo-worker && \
         export PISHOO_SSH_SESSION_BIN=/usr/lib/pishoo/pishoo-ssh-session && \
         cargo zigbuild --release --target {triple} -p {CARGO_NAME} --features sshd"
    );
    exec_in_container(docker, &container.id, &["bash", "-c", &build_cmd]).await?;

    // Stage + detect dependencies + build .deb
    let control = format_control(&[
        ("Package", CARGO_NAME),
        ("Version", &format!("{version}-1")),
        ("Architecture", arch),
        ("Maintainer", "Genmeta Tech Limited <support@genmeta.net>"),
        (
            "Description",
            "modern, secure, QUIC-powered web/proxy engine",
        ),
        ("Section", "httpd"),
        ("Priority", "optional"),
    ]);

    let package_script = format!(
        r#"set -e
# staging layout
mkdir -p /staging/usr/bin /staging/usr/lib/pishoo /staging/DEBIAN

cp /workspace/target/{triple}/release/pishoo /staging/usr/bin/
cp /workspace/target/{triple}/release/pishoo-worker /staging/usr/lib/pishoo/
cp /workspace/target/{triple}/release/pishoo-ssh-session /staging/usr/lib/pishoo/
chmod 755 /staging/usr/bin/* /staging/usr/lib/pishoo/*

# detect shared library dependencies for all binaries
DEPS=$(dpkg-shlibdeps -O \
    /staging/usr/bin/pishoo \
    /staging/usr/lib/pishoo/pishoo-worker \
    /staging/usr/lib/pishoo/pishoo-ssh-session \
    2>/dev/null | sed 's/^shlibs:Depends=//' || true)

# write control file
cat > /staging/DEBIAN/control <<'CTRL'
{control}CTRL

# append depends: pishoo-common + auto-detected
FULL_DEPS="pishoo-common (>= {COMMON_VERSION})"
if [ -n "$DEPS" ]; then
    FULL_DEPS="$FULL_DEPS, $DEPS"
fi
sed -i "/^Architecture:/a Depends: $FULL_DEPS" /staging/DEBIAN/control

# build .deb
dpkg-deb -b /staging /output/{deb_name}
"#
    );

    exec_in_container(docker, &container.id, &["mkdir", "-p", "/output"]).await?;
    exec_in_container(docker, &container.id, &["bash", "-c", &package_script]).await?;

    copy_from_container(
        docker,
        &container.id,
        &format!("/output/{deb_name}"),
        &out_dir,
    )
    .await?;

    docker
        .remove_container(
            &container.id,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .whatever_context("failed to remove build container")?;

    info!(deb_name, "produced");
    Ok(())
}

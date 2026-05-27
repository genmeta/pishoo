//! RPM (.rpm) packaging for pishoo via a Fedora 40 Docker container and
//! cargo-zigbuild.
//!
//! Produces two rpm packages per target triple:
//! * `pishoo-{version}-1.{rpm_arch}.rpm` — the arch-dependent package:
//!   `/usr/bin/pishoo`, `/usr/libexec/pishoo/pishoo-worker`, optional
//!   `/usr/libexec/pishoo/pishoo-ssh-session`.
//! * `pishoo-common-{version}-1.noarch.rpm` — the arch-independent package:
//!   `/etc/pishoo/*`, the systemd unit, and the `pishoo` system group.
//!
//! The spec is generated in Rust (no template file). Pishoo is built by
//! `cargo zigbuild` before `rpmbuild`; `%install` just copies pre-built
//! binaries staged into `SOURCES/`. Cross-arch PAM is provided by a
//! `dnf download --forcearch=<arch>`-extracted sysroot under
//! `/opt/sysroots/<arch>/`, referenced via `RUSTFLAGS=-L ...`.

use bollard::{
    Docker,
    models::{ContainerConfig, ContainerCreateBody, HostConfig, Mount, MountTypeEnum},
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
    },
};
use futures_util::StreamExt;
use snafu::{ResultExt, Whatever};
use tracing::{Instrument, info, info_span};

use crate::{
    Feature, RpmTarget,
    container::{
        CARGO_HOME, RUSTUP_HOME, Sibling, ZIG_GLIBC_VERSION, cargo_cache_mounts, check_docker,
        dhttp_bootstrap_from_env, exec_in_container, force_remove_container, host_uid_gid,
        remove_container_if_exists, resolve_siblings, start_container,
    },
    deb::PISHOO_LIBEXEC_DIR,
    package_meta, target_dir,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpmArtifact {
    pub target: String,
    pub path: std::path::PathBuf,
    pub features: Vec<String>,
}

const CARGO_NAME: &str = "pishoo";
const PACKAGE_NAME: &str = "pishoo";
const COMMON_PACKAGE_NAME: &str = "pishoo-common";

const BASE_IMAGE: &str = "fedora:40";
const IMAGE_TAG_PREFIX: &str = "pishoo-rpm-v1";

const RPM_LICENSE: &str = "Proprietary";
const RPM_VENDOR: &str = "Genmeta Tech Limited";
const RPM_URL: &str = "https://pishoo.genmeta.net";

const COMMON_FILES_DIR: &str = "xtask/deb/common";
const SYSTEMD_UNIT_SRC: &str = "xtask/deb/pishoo-common.pishoo.service";

fn rpm_arch(triple: &str) -> Result<&'static str, Whatever> {
    match triple {
        "x86_64-unknown-linux-gnu" => Ok("x86_64"),
        "aarch64-unknown-linux-gnu" => Ok("aarch64"),
        "armv7-unknown-linux-gnueabihf" => Ok("armv7hl"),
        "i686-unknown-linux-gnu" => Ok("i686"),
        _ => snafu::whatever!("unsupported rpm target triple: {triple}"),
    }
}

/// System lib dir inside the per-arch sysroot. Fedora places libpam/glibc
/// under `/usr/lib64` on 64-bit arches and `/usr/lib` on 32-bit.
fn sysroot_libdir(rpm_arch: &str) -> &'static str {
    match rpm_arch {
        "x86_64" | "aarch64" => "lib64",
        _ => "lib",
    }
}

pub async fn run(
    targets: &[RpmTarget],
    features: &[Feature],
    siblings: &[std::path::PathBuf],
) -> Result<Vec<RpmArtifact>, Whatever> {
    info!(target_count = targets.len(), "starting rpm dist build");
    let docker = Docker::connect_with_local_defaults()
        .whatever_context("failed to connect to Docker/Podman")?;
    check_docker(&docker).await?;

    let siblings = resolve_siblings(siblings)?;
    let meta = package_meta(CARGO_NAME)?;
    let target_dir = target_dir()?;

    let mut tasks = tokio::task::JoinSet::new();
    for &target in targets {
        let docker = docker.clone();
        let meta_version = meta.version.clone();
        let meta_description = meta.description.clone();
        let target_dir = target_dir.clone();
        let siblings = siblings.clone();
        let features = features.to_vec();
        let triple = target.triple();
        info!(triple, ?features, "queued rpm target build");
        let span = info_span!("rpm", triple);
        tasks.spawn(
            async move {
                build_one(
                    &docker,
                    triple,
                    &meta_version,
                    &meta_description,
                    &target_dir,
                    &features,
                    &siblings,
                )
                .await
            }
            .instrument(span),
        );
    }

    let mut artifacts = Vec::new();
    while let Some(result) = tasks.join_next().await {
        artifacts.extend(result.whatever_context("rpm build task panicked")??);
    }

    info!("finished rpm dist build");
    artifacts.sort_by(|left, right| {
        left.target
            .cmp(&right.target)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(artifacts)
}

async fn ensure_image(docker: &Docker, triple: &str) -> Result<String, Whatever> {
    let arch = rpm_arch(triple)?;
    let tag = format!("xtask-{triple}:{IMAGE_TAG_PREFIX}");
    if docker.inspect_image(&tag).await.is_ok() {
        info!(tag, "image already exists");
        return Ok(tag);
    }
    info!(tag, "building image");

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

    let container_name = format!("pishoo-xtask-rpm-setup-{triple}");
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
        .whatever_context("failed to create rpm setup container")?;
    let container_id = container.id.clone();

    let result = ensure_image_inner(docker, &container_id, triple, arch).await;
    if result.is_err() {
        force_remove_container(docker, &container_id).await;
        result?;
        unreachable!();
    }

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
    force_remove_container(docker, &container_id).await;
    commit_result?;

    info!(tag, "image ready");
    Ok(tag)
}

async fn ensure_image_inner(
    docker: &Docker,
    container_id: &str,
    triple: &str,
    rpm_arch: &str,
) -> Result<(), Whatever> {
    start_container(docker, container_id).await?;

    // Install rpmbuild stack, Rust nightly, Zig, cargo-zigbuild, and pull a
    // cross-arch PAM/glibc sysroot via `dnf download --forcearch` + rpm2cpio.
    // The host arch's pam/glibc are already installed by dnf so linking for
    // the native arch works without sysroot indirection; for foreign arches
    // the sysroot at /opt/sysroots/<arch>/ carries headers and shared libs.
    let sysroot_setup = format!(
        r#"mkdir -p /opt/sysroots/{rpm_arch}
cd /tmp
for pkg in pam-devel glibc-devel; do
    dnf download --forcearch={rpm_arch} --downloaddir=/tmp/rpms "$pkg"
done
cd /opt/sysroots/{rpm_arch}
for rpm in /tmp/rpms/*.{rpm_arch}.rpm /tmp/rpms/*.noarch.rpm; do
    [ -f "$rpm" ] || continue
    rpm2cpio "$rpm" | cpio -idmv 2>/dev/null
done
rm -rf /tmp/rpms"#
    );

    let setup_script = format!(
        r#"set -e
dnf install -y --setopt=install_weak_deps=False \
    rpm-build rpmdevtools systemd-rpm-macros \
    dnf-plugins-core cpio \
    pam-devel glibc-devel \
    gcc make pkgconf-pkg-config \
    ca-certificates curl wget tar xz which util-linux \
    clang

export CARGO_HOME={CARGO_HOME}
export RUSTUP_HOME={RUSTUP_HOME}
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- --default-toolchain nightly --profile minimal -y
export PATH="{CARGO_HOME}/bin:$PATH"
rustup target add {triple}

wget -q https://ziglang.org/download/0.14.0/zig-linux-x86_64-0.14.0.tar.xz
tar -xf zig-linux-x86_64-0.14.0.tar.xz
mv zig-linux-x86_64-0.14.0 /usr/local/zig
ln -s /usr/local/zig/zig /usr/local/bin/zig
rm zig-linux-x86_64-0.14.0.tar.xz

cargo install cargo-zigbuild

# cross-arch sysroot (PAM + glibc headers and libs)
{sysroot_setup}

chmod -R a+rX {CARGO_HOME} {RUSTUP_HOME} /opt/sysroots
"#
    );
    exec_in_container(docker, container_id, &["bash", "-c", &setup_script], None).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn build_one(
    docker: &Docker,
    triple: &str,
    version: &str,
    description: &str,
    target_dir: &std::path::Path,
    features: &[Feature],
    siblings: &[Sibling],
) -> Result<Vec<RpmArtifact>, Whatever> {
    let arch = rpm_arch(triple)?;
    let libdir = sysroot_libdir(arch);
    info!(triple, arch, "ensuring build image");
    let image = ensure_image(docker, triple).await?;

    let out_dir = target_dir.join(triple).join("release").join("rpm");
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
    for sibling in siblings {
        mounts.push(Mount {
            target: Some(format!("/{}", sibling.basename)),
            source: Some(sibling.host.to_string_lossy().into_owned()),
            typ: Some(MountTypeEnum::BIND),
            ..Default::default()
        });
    }
    mounts.extend(cargo_cache_mounts());

    let bootstrap = dhttp_bootstrap_from_env()?;
    mounts.extend(bootstrap.mounts);

    let container_name = format!("pishoo-xtask-rpm-{triple}");
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
        .whatever_context("failed to create rpm build container")?;
    let container_id = container.id.clone();

    let has_sshd = features
        .iter()
        .any(|f| matches!(f, Feature::Sshd | Feature::Pam));
    let has_pam = features.iter().any(|f| matches!(f, Feature::Pam));

    let result = build_one_inner(
        docker,
        &container_id,
        triple,
        version,
        description,
        arch,
        libdir,
        features,
        has_sshd,
        has_pam,
        &bootstrap.exports,
    )
    .await;
    force_remove_container(docker, &container_id).await;
    result?;

    info!(triple, out = %out_dir.display(), "produced rpm(s)");
    let mut artifacts = Vec::new();
    let mut entries = tokio::fs::read_dir(&out_dir)
        .await
        .whatever_context(format!("failed to read {}", out_dir.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .whatever_context(format!("failed to read entry in {}", out_dir.display()))?
    {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("rpm") {
            artifacts.push(RpmArtifact {
                target: triple.to_string(),
                path,
                features: crate::brew::feature_names(features),
            });
        }
    }
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(artifacts)
}

#[allow(clippy::too_many_arguments)]
async fn build_one_inner(
    docker: &Docker,
    container_id: &str,
    triple: &str,
    version: &str,
    description: &str,
    arch: &str,
    libdir: &str,
    features: &[Feature],
    has_sshd: bool,
    has_pam: bool,
    dhttp_bootstrap_exports: &str,
) -> Result<(), Whatever> {
    start_container(docker, container_id).await?;
    info!(triple, "build container started");

    let user = host_uid_gid()?;
    let spec = render_spec(version, description, has_sshd, has_pam);
    let spec_escaped = shell_escape(&spec);

    let feature_flag = if features.is_empty() {
        String::new()
    } else {
        let list = features
            .iter()
            .map(|f| match f {
                Feature::Sshd => "sshd",
                Feature::Pam => "pam",
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(" --features {list}")
    };

    // Pass libexec paths for pishoo's compile-time `option_env!` fallback so
    // the produced binary knows where to find pishoo-worker / ssh-session at
    // runtime without needing them to live beside pishoo itself.
    let worker_env = format!("export PISHOO_WORKER_BIN={PISHOO_LIBEXEC_DIR}/pishoo-worker\n");
    let ssh_session_env = if has_sshd {
        format!("export PISHOO_SSH_SESSION_BIN={PISHOO_LIBEXEC_DIR}/pishoo-ssh-session\n")
    } else {
        String::new()
    };

    let ssh_install = if has_sshd {
        r#"install -D -m 0755 "$TARGET_RELEASE/pishoo-ssh-session" \
    "$TOPDIR/SOURCES/pishoo-ssh-session"
"#
        .to_string()
    } else {
        String::new()
    };

    let build_script = format!(
        r#"set -e
export HOME=/tmp
export PATH="{CARGO_HOME}/bin:/usr/local/zig:$PATH"
export RUSTUP_HOME={RUSTUP_HOME}
export CARGO_HOME={CARGO_HOME}
{dhttp_bootstrap_exports}{worker_env}{ssh_session_env}
# Use per-arch sysroot for PAM + glibc when cross-compiling.
export RUSTFLAGS="${{RUSTFLAGS:-}} -L /opt/sysroots/{arch}/usr/{libdir}"

cd /workspace
cargo zigbuild --release --target {triple}.{ZIG_GLIBC_VERSION} -p pishoo{feature_flag}

TARGET_RELEASE=/workspace/target/{triple}/release
TOPDIR=/workspace/target/{triple}/release/rpm
rm -rf "$TOPDIR"/{{SPECS,BUILD,BUILDROOT,SOURCES,SRPMS,RPMS}}
mkdir -p "$TOPDIR"/{{SPECS,BUILD,BUILDROOT,SOURCES,SRPMS,RPMS}}

SPEC="$TOPDIR/SPECS/pishoo.spec"
printf '%s' {spec_escaped} > "$SPEC"

install -D -m 0755 "$TARGET_RELEASE/pishoo"         "$TOPDIR/SOURCES/pishoo"
install -D -m 0755 "$TARGET_RELEASE/pishoo-worker"  "$TOPDIR/SOURCES/pishoo-worker"
{ssh_install}
install -D -m 0644 /workspace/{COMMON_FILES_DIR}/etc/pishoo/pishoo.conf \
    "$TOPDIR/SOURCES/pishoo.conf"
install -D -m 0644 /workspace/{COMMON_FILES_DIR}/etc/pishoo/mime.types \
    "$TOPDIR/SOURCES/mime.types"
install -D -m 0644 /workspace/{SYSTEMD_UNIT_SRC} \
    "$TOPDIR/SOURCES/pishoo.service"

rpmbuild -bb \
    --target={arch} \
    --define "_topdir $TOPDIR" \
    --define "_binary_payload w19.xzdio" \
    "$SPEC"

find "$TOPDIR/RPMS" -name '*.rpm' -exec mv {{}} "$TOPDIR/" \;
"#
    );

    exec_in_container(
        docker,
        container_id,
        &["bash", "-c", &build_script],
        Some(&user),
    )
    .await?;
    info!(triple, "rpmbuild finished inside container");
    Ok(())
}

fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Render the pishoo spec with a `pishoo-common` noarch subpackage.
///
/// The spec deliberately sets `AutoReqProv: no` on both packages and declares
/// runtime requirements by hand: rpm's dependency generator would otherwise
/// pin libc symbol versions from the zig-bundled glibc shim, which do not
/// match the target distro's glibc.
fn render_spec(version: &str, description: &str, has_sshd: bool, has_pam: bool) -> String {
    let desc = if description.is_empty() {
        "Pishoo QUIC-powered, peer-to-peer web/proxy engine."
    } else {
        description
    };

    // Source and file entries, numbered. Kept contiguous so indices line up with
    // `install` lines in the %install scriptlet.
    let mut source_lines = String::new();
    let mut file_lines = String::new();
    let mut install_lines = String::new();
    let mut source_idx = 0u32;

    // pishoo main binary
    source_lines.push_str(&format!("Source{source_idx}:        pishoo\n"));
    install_lines.push_str(&format!(
        "install -D -m 0755 %{{SOURCE{source_idx}}} %{{buildroot}}/usr/bin/pishoo\n"
    ));
    file_lines.push_str("/usr/bin/pishoo\n");
    source_idx += 1;

    // pishoo-worker
    source_lines.push_str(&format!("Source{source_idx}:        pishoo-worker\n"));
    install_lines.push_str(&format!(
        "install -D -m 0755 %{{SOURCE{source_idx}}} %{{buildroot}}{PISHOO_LIBEXEC_DIR}/pishoo-worker\n"
    ));
    file_lines.push_str(&format!("{PISHOO_LIBEXEC_DIR}/pishoo-worker\n"));
    source_idx += 1;

    // optional pishoo-ssh-session
    if has_sshd {
        source_lines.push_str(&format!("Source{source_idx}:        pishoo-ssh-session\n"));
        install_lines.push_str(&format!(
            "install -D -m 0755 %{{SOURCE{source_idx}}} %{{buildroot}}{PISHOO_LIBEXEC_DIR}/pishoo-ssh-session\n"
        ));
        file_lines.push_str(&format!("{PISHOO_LIBEXEC_DIR}/pishoo-ssh-session\n"));
        source_idx += 1;
    }

    // common subpackage sources
    let common_conf_src = source_idx;
    source_lines.push_str(&format!("Source{source_idx}:        pishoo.conf\n"));
    source_idx += 1;
    let common_mime_src = source_idx;
    source_lines.push_str(&format!("Source{source_idx}:        mime.types\n"));
    source_idx += 1;
    let common_unit_src = source_idx;
    source_lines.push_str(&format!("Source{source_idx}:        pishoo.service\n"));
    // source_idx no longer used after this point

    let common_install = format!(
        r#"install -D -m 0644 %{{SOURCE{common_conf_src}}} %{{buildroot}}/etc/pishoo/pishoo.conf
install -D -m 0644 %{{SOURCE{common_mime_src}}} %{{buildroot}}/etc/pishoo/mime.types
install -D -m 0644 %{{SOURCE{common_unit_src}}} %{{buildroot}}%{{_unitdir}}/pishoo.service
"#
    );

    let pam_req = if has_pam { "Requires:       pam\n" } else { "" };

    format!(
        r#"Name:           {PACKAGE_NAME}
Version:        {version}
Release:        1%{{?dist}}
Summary:        Modern, secure, QUIC-powered web/proxy engine
License:        {RPM_LICENSE}
URL:            {RPM_URL}
Vendor:         {RPM_VENDOR}
{source_lines}AutoReqProv:    no
Requires:       {COMMON_PACKAGE_NAME} = %{{version}}-%{{release}}
Requires:       glibc
Requires:       systemd
{pam_req}BuildRequires:  systemd-rpm-macros

%description
{desc}

%package common
Summary:        Common files for pishoo
License:        {RPM_LICENSE}
BuildArch:      noarch
AutoReqProv:    no
Requires(pre):  shadow-utils

%description common
Common configuration files and the systemd unit for the pishoo proxy engine.

%prep
# nothing to do: binaries are pre-built by cargo-zigbuild

%build
# nothing to do: binaries are pre-built by cargo-zigbuild

%install
rm -rf %{{buildroot}}
{install_lines}{common_install}

%files
{file_lines}
%files common
%dir /etc/pishoo
%config(noreplace) /etc/pishoo/pishoo.conf
/etc/pishoo/mime.types
%{{_unitdir}}/pishoo.service

%pre common
getent group pishoo >/dev/null || groupadd --system pishoo

%post
%systemd_post pishoo.service

%preun
%systemd_preun pishoo.service

%postun
%systemd_postun_with_restart pishoo.service

%changelog
* %(date '+%a %b %d %Y') {RPM_VENDOR} <support@genmeta.net> - {version}-1
- release {version}
"#
    )
}

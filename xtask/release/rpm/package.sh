#!/usr/bin/env bash
set -euo pipefail

PISHOO_LIBEXEC_DIR=/usr/libexec/pishoo

prepare_product_source() {
    local dest=$1
    rm -rf "$dest"
    mkdir -p "$dest"
    tar -C "${XTASK_RELEASE_REPO_ROOT:?}" \
        --exclude='./.git' \
        --exclude='./target' \
        --exclude='./xtask' \
        -cf - . | tar -C "$dest" -xf -
    python3 - "$dest/Cargo.toml" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
text = path.read_text()
text = text.replace('members = ["gateway", "pishoo", "xtask"]', 'members = ["gateway", "pishoo"]')
path.write_text(text)
PY
}

rpm_arch() {
    case "$1" in
        x86_64-unknown-linux-gnu) echo x86_64 ;;
        aarch64-unknown-linux-gnu) echo aarch64 ;;
        armv7-unknown-linux-gnueabihf) echo armv7hl ;;
        i686-unknown-linux-gnu) echo i686 ;;
        *) echo "unsupported rpm target $1" >&2; exit 1 ;;
    esac
}

sysroot_libdir() {
    case "$1" in
        x86_64|aarch64) echo lib64 ;;
        *) echo lib ;;
    esac
}

write_aarch64_zig_workaround() {
    if [ "${XTASK_RELEASE_TARGET:?}" != "aarch64-unknown-linux-gnu" ]; then
        return
    fi
    cat > /tmp/pishoo-aarch64-zig <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "cc" ] || [ "${1:-}" = "c++" ]; then
    zig_subcommand="$1"
    shift
    filtered_args=()
    for arg in "$@"; do
        case "$arg" in
            -Wl,--fix-cortex-a53-843419|--fix-cortex-a53-843419) continue ;;
        esac
        filtered_args+=("$arg")
    done
    exec /usr/local/zig/zig "$zig_subcommand" "${filtered_args[@]}"
fi
exec /usr/local/zig/zig "$@"
SH
    chmod +x /tmp/pishoo-aarch64-zig
    export CARGO_ZIGBUILD_ZIG_PATH=/tmp/pishoo-aarch64-zig
    export RUSTFLAGS="${RUSTFLAGS:-} -Z unstable-options -Clinker-flavor=gnu-lld-cc"
}

split_package_version() {
    local package_version=${XTASK_RELEASE_PACKAGE_VERSION:?}
    if [ "$package_version" = "${package_version%-*}" ]; then
        echo "rpm package version must include release suffix: $package_version" >&2
        exit 1
    fi
    RPM_VERSION=${package_version%-*}
    RPM_RELEASE=${package_version##*-}
}

write_common_spec() {
    cat > "$1" <<EOF_SPEC
Name:           pishoo-common
Version:        ${RPM_VERSION}
Release:        ${RPM_RELEASE}
Summary:        Common files for pishoo
License:        Apache-2.0
URL:            https://www.dhttp.net
Vendor:         Genmeta Tech Limited
Source0:        pishoo.conf
Source1:        mime.types
Source2:        pishoo.service
BuildArch:      noarch
AutoReqProv:    no
Requires(pre):  shadow-utils
Requires:       systemd
BuildRequires:  systemd-rpm-macros

%description
Common configuration files and the systemd unit for the pishoo proxy engine.

%prep
# nothing to do: common files are staged into SOURCES

%build
# nothing to do: common files are staged into SOURCES

%install
rm -rf %{buildroot}
install -D -m 0644 %{SOURCE0} %{buildroot}/etc/dhttp/pishoo.conf
install -D -m 0644 %{SOURCE1} %{buildroot}/etc/dhttp/mime.types
install -D -m 0644 %{SOURCE2} %{buildroot}%{_unitdir}/pishoo.service

%files
%dir /etc/dhttp
%config(noreplace) /etc/dhttp/pishoo.conf
/etc/dhttp/mime.types
%{_unitdir}/pishoo.service

%pre
if getent group dhttp >/dev/null; then
    :
else
    status=\$?
    if [ "\$status" -ne 2 ]; then
        exit "\$status"
    fi
    groupadd --system dhttp
fi

%post
%systemd_post pishoo.service

%preun
%systemd_preun pishoo.service

%postun
%systemd_postun_with_restart pishoo.service

%changelog
* %(date '+%a %b %d %Y') Genmeta Tech Limited <developer@genmeta.net> - ${RPM_VERSION}-${RPM_RELEASE}
- release ${XTASK_RELEASE_SOURCE_VERSION:?}
EOF_SPEC
}

write_binary_spec() {
    local has_sshd=$1
    local has_pam=$2
    local source_lines=''
    local install_lines=''
    local file_lines=''
    local pam_req=''
    source_lines+='Source0:        pishoo
'
    source_lines+='Source1:        pishoo-worker
'
    install_lines+='install -D -m 0755 %{SOURCE0} %{buildroot}/usr/bin/pishoo
'
    install_lines+="install -D -m 0755 %{SOURCE1} %{buildroot}${PISHOO_LIBEXEC_DIR}/pishoo-worker"$'\n'
    file_lines+='/usr/bin/pishoo
'
    file_lines+="${PISHOO_LIBEXEC_DIR}/pishoo-worker"$'\n'
    if [ "$has_sshd" = true ]; then
        source_lines+='Source2:        pishoo-ssh-session
'
        install_lines+="install -D -m 0755 %{SOURCE2} %{buildroot}${PISHOO_LIBEXEC_DIR}/pishoo-ssh-session"$'\n'
        file_lines+="${PISHOO_LIBEXEC_DIR}/pishoo-ssh-session"$'\n'
    fi
    if [ "$has_pam" = true ]; then
        pam_req='Requires:       pam'
    fi
    cat > "$3" <<EOF_SPEC
Name:           pishoo
Version:        ${RPM_VERSION}
Release:        ${RPM_RELEASE}
Summary:        Modern, secure, QUIC-powered web/proxy engine
License:        Apache-2.0
URL:            https://www.dhttp.net
Vendor:         Genmeta Tech Limited
${source_lines}AutoReqProv:    no
${RPM_REQUIRES_LINES:?}
Requires:       glibc
${pam_req}
%description
${XTASK_RELEASE_DESCRIPTION:-Pishoo QUIC-powered, peer-to-peer web/proxy engine.}

%prep
# nothing to do: binaries are pre-built by cargo-zigbuild

%build
# nothing to do: binaries are pre-built by cargo-zigbuild

%install
rm -rf %{buildroot}
${install_lines}
%files
${file_lines}
%changelog
* %(date '+%a %b %d %Y') Genmeta Tech Limited <developer@genmeta.net> - ${RPM_VERSION}-${RPM_RELEASE}
- release ${XTASK_RELEASE_SOURCE_VERSION:?}
EOF_SPEC
}

rpm_requires_lines() {
    python3 - <<'PY'
import os
requires = os.environ.get('XTASK_RELEASE_REQUIRES', '')
for item in [part.strip() for part in requires.split(',') if part.strip()]:
    print(f'Requires:       {item}')
PY
}

run_common() {
    split_package_version
    cd "${XTASK_RELEASE_REPO_ROOT:?}"
    topdir="${XTASK_RELEASE_OUT_DIR:?}/rpmbuild"
    rm -rf "$topdir"/{SPECS,BUILD,BUILDROOT,SOURCES,SRPMS,RPMS}
    mkdir -p "$topdir"/{SPECS,BUILD,BUILDROOT,SOURCES,SRPMS,RPMS}
    spec="$topdir/SPECS/pishoo-common.spec"
    write_common_spec "$spec"
    install -D -m 0644 xtask/deb/common/etc/dhttp/pishoo.conf "$topdir/SOURCES/pishoo.conf"
    install -D -m 0644 xtask/deb/common/etc/dhttp/mime.types "$topdir/SOURCES/mime.types"
    install -D -m 0644 xtask/deb/pishoo-common.pishoo.service "$topdir/SOURCES/pishoo.service"
    rpmbuild -bb \
        --define "_topdir $topdir" \
        --define "_binary_payload w19.xzdio" \
        "$spec"
    find "$topdir/RPMS" -name '*.rpm' -exec mv {} "${XTASK_RELEASE_OUT_DIR:?}/" \;
}

run_binary() {
    local target=${XTASK_RELEASE_TARGET:?}
    local arch
    arch=$(rpm_arch "$target")
    local libdir
    libdir=$(sysroot_libdir "$arch")
    local profile=${XTASK_RELEASE_PROFILE:?}
    local profile_args=
    if [ "$profile" = "release" ]; then
        profile_args=--release
    fi
    local features=${XTASK_RELEASE_FEATURES:-}
    local feature_flag=()
    if [ -n "$features" ]; then
        feature_flag=(--features "$features")
    fi
    local has_sshd=false
    local has_pam=false
    case ",$features," in
        *,sshd,*|*,pam,*) has_sshd=true ;;
    esac
    case ",$features," in
        *,pam,*) has_pam=true ;;
    esac

    split_package_version
    export RPM_REQUIRES_LINES
    RPM_REQUIRES_LINES=$(rpm_requires_lines)

    local src="${XTASK_RELEASE_OUT_DIR:?}/src"
    local product_source="$src/product-source"
    prepare_product_source "$product_source"

    export HOME=/tmp
    export RUSTUP_HOME=/opt/rustup
    export CARGO_HOME=/opt/cargo
    export PATH=/opt/cargo/bin:/usr/local/zig:$PATH
    export RUSTFLAGS="${RUSTFLAGS:-} -L /opt/sysroots/$arch/usr/$libdir"
    export PISHOO_WORKER_BIN=${PISHOO_LIBEXEC_DIR}/pishoo-worker
    if [ "$has_sshd" = true ]; then
        export PISHOO_SSH_SESSION_BIN=${PISHOO_LIBEXEC_DIR}/pishoo-ssh-session
    fi
    write_aarch64_zig_workaround

    cd "$product_source"
    cargo zigbuild $profile_args --target "$target" -p pishoo "${feature_flag[@]}"

    local release_dir="$product_source/target/$target/$profile"
    local topdir="${XTASK_RELEASE_OUT_DIR:?}/rpmbuild"
    rm -rf "$topdir"/{SPECS,BUILD,BUILDROOT,SOURCES,SRPMS,RPMS}
    mkdir -p "$topdir"/{SPECS,BUILD,BUILDROOT,SOURCES,SRPMS,RPMS}

    local spec="$topdir/SPECS/pishoo.spec"
    write_binary_spec "$has_sshd" "$has_pam" "$spec"
    install -D -m 0755 "$release_dir/pishoo" "$topdir/SOURCES/pishoo"
    install -D -m 0755 "$release_dir/pishoo-worker" "$topdir/SOURCES/pishoo-worker"
    if [ "$has_sshd" = true ]; then
        install -D -m 0755 "$release_dir/pishoo-ssh-session" "$topdir/SOURCES/pishoo-ssh-session"
    fi

    rpmbuild -bb \
        --target="$arch" \
        --define "_topdir $topdir" \
        --define "_binary_payload w19.xzdio" \
        "$spec"
    find "$topdir/RPMS" -name '*.rpm' -exec mv {} "${XTASK_RELEASE_OUT_DIR:?}/" \;
}

case "${XTASK_RELEASE_PACKAGE_ID:?}" in
    pishoo-common) run_common ;;
    pishoo) run_binary ;;
    *) echo "pishoo rpm script received unexpected package ${XTASK_RELEASE_PACKAGE_ID}" >&2; exit 1 ;;
esac

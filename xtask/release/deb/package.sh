#!/usr/bin/env bash
set -euo pipefail

render_control() {
    python3 - "$1" <<'PY'
from pathlib import Path
import sys
replacement = sys.argv[1]
control = Path('xtask/deb/control').read_text()
if replacement:
    control = control.replace('{{PISHOO_COMMON_DEPENDS}}', replacement)
else:
    control = control.replace('{{PISHOO_COMMON_DEPENDS}}, ', '')
Path(sys.argv[2] if len(sys.argv) > 2 else '/dev/stdout').write_text(control)
PY
}

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

cd "${XTASK_RELEASE_REPO_ROOT:?}"
package=${XTASK_RELEASE_PACKAGE_ID:?}
target=${XTASK_RELEASE_TARGET:?}
profile=${XTASK_RELEASE_PROFILE:?}
profile_args=
if [ "$profile" = "release" ]; then
    profile_args=--release
fi
src="${XTASK_RELEASE_OUT_DIR:?}/src"
rm -rf "$src"
mkdir -p "$src/debian"
cp -r xtask/deb/. "$src/debian/"

if [ "$package" = "pishoo-common" ]; then
    render_control "" > "$src/debian/control"
    printf 'pishoo (%s) unstable; urgency=low\n\n  * release %s\n\n -- Genmeta Tech Limited <developer@genmeta.net>  %s\n' \
        "${XTASK_RELEASE_PACKAGE_VERSION:?}" "${XTASK_RELEASE_SOURCE_VERSION:?}" "$(date -R)" > "$src/debian/changelog"
    cd "$src"
    dpkg-buildpackage -A -uc -us -d
    exit 0
fi

if [ "$package" != "pishoo" ]; then
    echo "pishoo deb script received unexpected package $package" >&2
    exit 1
fi

case "$target" in
    x86_64-unknown-linux-gnu) deb=amd64; gnu=x86_64-linux-gnu ;;
    aarch64-unknown-linux-gnu) deb=arm64; gnu=aarch64-linux-gnu ;;
    armv7-unknown-linux-gnueabihf) deb=armhf; gnu=arm-linux-gnueabihf ;;
    i686-unknown-linux-gnu) deb=i386; gnu=i386-linux-gnu ;;
    *) echo "unsupported deb target $target" >&2; exit 1 ;;
esac

render_control "${XTASK_RELEASE_REQUIRES:-}" > "$src/debian/control"
printf 'pishoo (%s) unstable; urgency=low\n\n  * release %s\n\n -- Genmeta Tech Limited <developer@genmeta.net>  %s\n' \
    "${XTASK_RELEASE_PACKAGE_VERSION:?}" "${XTASK_RELEASE_SOURCE_VERSION:?}" "$(date -R)" > "$src/debian/changelog"
product_source="$src/product-source"
prepare_product_source "$product_source"

features=${XTASK_RELEASE_FEATURES:-}
if [ -n "$features" ]; then
    export CARGO_FEATURES=$features
fi
case ",$features," in
    *,sshd,*|*,pam,*) export PISHOO_SSH_SESSION_BIN=/usr/libexec/pishoo/pishoo-ssh-session ;;
esac
export PISHOO_WORKER_BIN=/usr/libexec/pishoo/pishoo-worker
export HOME=/tmp
export RUSTUP_HOME=/opt/rustup
export CARGO_HOME=/opt/cargo
export PATH=/opt/cargo/bin:/usr/local/zig:$PATH
export TRIPLE=$target
export ZIG_TARGET=$target
export BUILD_PROFILE=$profile
export CARGO_PROFILE_ARGS=$profile_args
export DEB_HOST_MULTIARCH=$gnu
export SOURCE_ROOT=$product_source

cd "$src"
dpkg-buildpackage -B -uc -us -d -a"$deb"

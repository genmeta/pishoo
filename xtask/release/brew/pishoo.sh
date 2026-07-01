#!/usr/bin/env bash
set -euo pipefail

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
target=${XTASK_RELEASE_TARGET:?}
out=${XTASK_RELEASE_OUT_DIR:?}
version=${XTASK_RELEASE_SOURCE_VERSION:?}
features=${XTASK_RELEASE_FEATURES:-}
rm -rf "$out/staging"
mkdir -p "$out/staging"
product_source="$out/product-source"
prepare_product_source "$product_source"
args=(build --release --manifest-path "$product_source/pishoo/Cargo.toml" --target "$target")
if [ -n "$features" ]; then
    args+=(--features "$features")
fi
cargo "${args[@]}"
release_dir="$product_source/target/$target/release"
cp "$release_dir/pishoo" "$out/staging/pishoo"
cp "$release_dir/pishoo-worker" "$out/staging/pishoo-worker"
case ",$features," in
    *,sshd,*|*,pam,*) cp "$release_dir/pishoo-ssh-session" "$out/staging/pishoo-ssh-session" ;;
esac
cp xtask/deb/common/etc/dhttp/pishoo.conf "$out/staging/pishoo.conf"
cp xtask/deb/common/etc/dhttp/mime.types "$out/staging/mime.types"
sed -i.bak 's#/etc#etc#g' "$out/staging/pishoo.conf"
rm -f "$out/staging/pishoo.conf.bak"
tar -C "$out/staging" -czf "$out/pishoo_$version-$target.tar.gz" .
rm -rf "$out/staging"

#!/bin/sh
set -eu
journal_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to the official WASI SDK directory}"
if [ "$#" -ne 0 ] && { [ "$#" -ne 2 ] || [ "$1" != "--publisher" ]; }; then
  echo "usage: build.sh [--publisher NAME]" >&2
  exit 2
fi
# The project manifests repeat package identity/version for their respective tools.
# Refuse a release with inconsistent names or versions before compiling/signing.
journal_version=$(python3 - "$journal_dir" <<'PY'
import json
import sys
import tomllib
from pathlib import Path
root = Path(sys.argv[1])
cargo = tomllib.loads((root / 'Cargo.toml').read_text())['package']
package = tomllib.loads((root / 'wasmer.toml').read_text())
document = json.loads((root / 'openapi.json').read_text())
release = document['x-pishoo']
if not (cargo['version'] == package['package']['version'] == document['info']['version']):
    sys.exit('Cargo, wasmer.toml and OpenAPI versions must match')
if cargo['name'] != release['name'] or package['package']['name'] != release['publisher'] + '/' + release['name']:
    sys.exit('Cargo, wasmer.toml and OpenAPI package names must match')
if package['module'] != [{'name': 'backend', 'source': 'lib.wasm'}] or package['fs'] != {'/public': 'public', '/metadata': 'metadata'}:
    sys.exit('Expected one backend and local public/metadata mappings')
print(cargo['version'])
PY
)
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang"
export CFLAGS_wasm32_wasip2="--target=wasm32-wasip2 -USQLITE_THREADSAFE -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_WAL -DSQLITE_TEMP_STORE=3"
cargo build --locked --release --target wasm32-wasip2 --target-dir "$journal_dir/target" --manifest-path "$journal_dir/Cargo.toml"
cp "$journal_dir/target/wasm32-wasip2/release/journal.wasm" "$journal_dir/lib.wasm"
wasm-tools validate "$journal_dir/lib.wasm"
mkdir -p "$journal_dir/metadata" "$journal_dir/dist"
cp "$journal_dir/openapi.json" "$journal_dir/metadata/openapi.json"
if [ "$#" -eq 0 ]; then
  echo "Built backend lib.wasm and metadata/openapi.json. Use --publisher NAME to create a signed WEBC release."
  exit 0
fi
# The installed tool works in the standalone SDK bundle as well as this checkout.
if [ -n "${PISHOO_PACKAGE_WEBC:-}" ]; then
  "$PISHOO_PACKAGE_WEBC" "$journal_dir/lib.wasm" "$journal_dir/openapi.json" "$journal_dir/public" "$journal_dir/dist/journal-$journal_version.webc" "$@"
else
  repo_dir=$(CDPATH= cd -- "$journal_dir/../../.." && pwd)
  if [ ! -f "$repo_dir/pishoo/tools/package-webc.rs" ]; then
    echo "Set PISHOO_PACKAGE_WEBC to the package-webc executable to sign this standalone project." >&2
    exit 1
  fi
  cargo run --locked --manifest-path "$repo_dir/Cargo.toml" -p pishoo --example package-webc -- \
    "$journal_dir/lib.wasm" "$journal_dir/openapi.json" "$journal_dir/public" "$journal_dir/dist/journal-$journal_version.webc" "$@"
fi

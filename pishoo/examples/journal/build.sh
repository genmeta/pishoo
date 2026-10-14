#!/bin/sh
set -eu
journal_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$#" -ne 0 ] && { [ "$#" -ne 2 ] || [ "$1" != "--publisher" ]; }; then
  echo "usage: build.sh [--publisher NAME]" >&2
  exit 2
fi
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to the official WASI SDK directory}"
# Check both independent package manifests before building or signing.
journal_version=$(python3 - "$journal_dir" "${2:-}" <<'PY'
import json
import sys
import tomllib
from pathlib import Path
root = Path(sys.argv[1])
cargo = tomllib.loads((root / 'Cargo.toml').read_text())['package']
lib = tomllib.loads((root / 'wasmer.toml').read_text())
app = tomllib.loads((root / 'app.toml').read_text())
document = json.loads((root / 'openapi.json').read_text())
version = cargo['version']
if not (version == lib['package']['version'] == app['package']['version'] == document['info']['version']):
    sys.exit('Cargo, App/Lib manifests and OpenAPI versions must match')
publisher, name = lib['package']['name'].split('/')
if cargo['name'] != name or app['package']['name'] != lib['package']['name']:
    sys.exit('Cargo and App/Lib package names must match')
if sys.argv[2] and sys.argv[2] != publisher:
    sys.exit('--publisher must match the publisher in both manifests')
if lib['package']['pishoo']['kind'] != 'lib' or lib['package']['pishoo']['dependencies'] != []:
    sys.exit('Journal Lib must have kind=lib and no backend dependencies')
if lib['module'] != [{'name': 'backend', 'source': 'lib.wasm'}] or lib['fs'] != {'/metadata': 'metadata'}:
    sys.exit('Lib requires one backend and only metadata mapping')
if app.get('module') or app['fs'] != {'/public': 'public'}:
    sys.exit('App requires zero backend modules and only public mapping')
if app['package']['pishoo']['kind'] != 'app' or app['package']['pishoo']['entry'] != '/':
    sys.exit('Journal App must have kind=app and entry=/')
if app['package']['pishoo']['dependencies'] != [{'publisher': publisher, 'name': name, 'version': version}]:
    sys.exit('App must depend on the exact Journal Lib release')
if set(document['x-pishoo']) != {'access-groups'} or any(not path.startswith('/journal/') for path in document['paths']):
    sys.exit('OpenAPI must contain only Journal business paths and access groups')
if sys.argv[2]:
    for kind in ('lib', 'app'):
        output = root / 'dist' / f'journal-{version}-{kind}.webc'
        if output.exists():
            sys.exit(f'Output already exists: {output}; choose a new version or move the prior local build')
print(version)
PY
)
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang"
export CFLAGS_wasm32_wasip2="--target=wasm32-wasip2 -USQLITE_THREADSAFE -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_WAL -DSQLITE_TEMP_STORE=3"
cd "${TMPDIR:-/tmp}"
cargo build --locked --release --target wasm32-wasip2 --target-dir "$journal_dir/target" --manifest-path "$journal_dir/Cargo.toml"
cp "$journal_dir/target/wasm32-wasip2/release/journal.wasm" "$journal_dir/lib.wasm"
wasm-tools validate "$journal_dir/lib.wasm"
mkdir -p "$journal_dir/metadata" "$journal_dir/dist"
cp "$journal_dir/openapi.json" "$journal_dir/metadata/openapi.json"
if [ "$#" -eq 0 ]; then
  echo "Built Journal backend and metadata. Use --publisher NAME to sign independent Lib and App releases."
  exit 0
fi
if [ -n "${PISHOO_PACKAGE_WEBC:-}" ]; then
  tool=$PISHOO_PACKAGE_WEBC
else
  repo_dir=$(CDPATH= cd -- "$journal_dir/../../.." && pwd)
  if [ ! -f "$repo_dir/pishoo/tools/package-webc.rs" ]; then
    echo "Set PISHOO_PACKAGE_WEBC to the pishoo-pack executable to sign this standalone project." >&2
    exit 1
  fi
  cargo build --locked --manifest-path "$repo_dir/Cargo.toml" -p pishoo --bin pishoo-pack
  tool="$repo_dir/target/debug/pishoo-pack"
fi
"$tool" "$journal_dir/wasmer.toml" -o "$journal_dir/dist/journal-$journal_version-lib.webc" --id "$2"
"$tool" "$journal_dir/app.toml" -o "$journal_dir/dist/journal-$journal_version-app.webc" --id "$2"
echo "Publish journal-$journal_version-lib.webc first, then journal-$journal_version-app.webc."

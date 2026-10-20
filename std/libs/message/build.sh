#!/bin/sh
set -eu
project=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to the official WASI SDK directory}"
publisher=${PISHOO_WORKSPACE_PUBLISHER:-dev.dhttp.net}
if [ "$#" -gt 0 ]; then
  [ "$#" -eq 2 ] && [ "$1" = "--publisher" ] || { echo 'usage: build.sh [--publisher NAME]' >&2; exit 2; }
  publisher=$2
fi
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang"
export CFLAGS_wasm32_wasip2="--target=wasm32-wasip2 -USQLITE_THREADSAFE -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_WAL -DSQLITE_TEMP_STORE=3"
cd "${TMPDIR:-/tmp}"
cargo build --locked --release --target wasm32-wasip2 --manifest-path "$project/Cargo.toml"
mkdir -p "$project/dist/metadata"
cp "$project/target/wasm32-wasip2/release/message.wasm" "$project/dist/lib.wasm"
python3 - "$project" "$publisher" <<'PYTHON'
from pathlib import Path
import json,sys,re
root=Path(sys.argv[1]);publisher=sys.argv[2]
manifest=(root/'wasmer.toml').read_text().replace('dev.dhttp.net',publisher)
version=re.search(r'^version = "([^"]+)"', (root/'Cargo.toml').read_text(), re.M).group(1)
assert re.search(r'^version = "([^"]+)"', manifest, re.M).group(1)==version
api=json.loads((root/'openapi.json').read_text()); assert api['info']['version']==version
(root/'dist/wasmer.toml').write_text(manifest)
(root/'dist/metadata/openapi.json').write_text(json.dumps(api,ensure_ascii=False,indent=2)+'\n')
PYTHON
python3 "$project/../../../pishoo/tools/package-lib.py" "$project/dist/lib.wasm" "$project/dist/metadata/openapi.json" "$project/dist/lib.wasm"
rm "$project/dist/metadata/openapi.json"
wasm-tools validate "$project/dist/lib.wasm"
echo "Built message backend with embedded OpenAPI: $project/dist/lib.wasm"

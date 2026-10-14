#!/bin/sh
set -eu
project=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
publisher=${PISHOO_WORKSPACE_PUBLISHER:-dev.dhttp.net}
if [ "$#" -gt 0 ]; then
  [ "$#" -eq 2 ] && [ "$1" = "--publisher" ] || { echo 'usage: build.sh [--publisher NAME]' >&2; exit 2; }
  publisher=$2
fi
cd "$project"
bun install --frozen-lockfile
bun run build
python3 - "$project" "$publisher" <<'PY'
from pathlib import Path
import json,sys
root=Path(sys.argv[1]);publisher=sys.argv[2]
manifest=(root/'wasmer.toml').read_text().replace('dev.dhttp.net',publisher)
import re
assert re.search(r'^version = "([^"]+)"', manifest, re.M).group(1)==json.loads((root/'package.json').read_text())['version']
(root/'package-dist').mkdir(exist_ok=True)
# The source input lives beside public, outside the actual public tree.
manifest=manifest.replace('"/public" = "."','"/public" = "../dist"')
(root/'package-dist/wasmer.toml').write_text(manifest)
PY
echo "Built separate frontend: $project/dist"

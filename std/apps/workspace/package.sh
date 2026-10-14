#!/bin/sh
set -eu
project=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH= cd -- "$project/../../.." && pwd)
[ "$#" -eq 2 ] && [ "$1" = "--publisher" ] || { echo 'usage: package.sh --publisher NAME' >&2; exit 2; }
publisher=$2
sh "$repo/std/libs/message/build.sh" --publisher "$publisher"
sh "$repo/std/libs/profile/build.sh" --publisher "$publisher"
sh "$project/build.sh" --publisher "$publisher"
output=${PISHOO_WORKSPACE_DIST:-"$repo/target/workspace-dist"}
mkdir -p "$output/default-artifacts"
if [ -n "${PISHOO_PACKAGE_WEBC:-}" ]; then tool=$PISHOO_PACKAGE_WEBC
else
  cd "${TMPDIR:-/tmp}"
  cargo build --locked --manifest-path "$repo/Cargo.toml" -p pishoo --example package-webc
  tool="$repo/target/debug/examples/package-webc"
fi
"$tool" "$repo/std/libs/message/dist/wasmer.toml" "$output/default-artifacts/message-lib.webc" --publisher "$publisher"
"$tool" "$repo/std/libs/profile/dist/wasmer.toml" "$output/default-artifacts/profile-lib.webc" --publisher "$publisher"
"$tool" "$project/package-dist/wasmer.toml" "$output/default-artifacts/workspace-app.webc" --publisher "$publisher"
echo "Three ordinary artifacts: $output/default-artifacts"

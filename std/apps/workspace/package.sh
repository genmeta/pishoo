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
mkdir -p "$output"
tool=${GENMETA:-genmeta}
"$tool" package build --no-build "$repo/std/libs/message/dist/wasmer.toml" -o "$output/message-lib.webc" --id "$publisher"
"$tool" package build --no-build "$repo/std/libs/profile/dist/wasmer.toml" -o "$output/profile-lib.webc" --id "$publisher"
"$tool" package build --no-build "$project/package-dist/wasmer.toml" -o "$output/workspace-app.webc" --id "$publisher"
echo "Three ordinary artifacts: $output"

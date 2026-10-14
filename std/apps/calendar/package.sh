#!/bin/sh
set -eu
project=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH= cd -- "$project/../../.." && pwd)
[ "$#" -eq 2 ] && [ "$1" = "--publisher" ] || { echo 'usage: package.sh --publisher NAME' >&2; exit 2; }
publisher=$2
sh "$repo/std/libs/calendar/build.sh" --publisher "$publisher"
sh "$project/build.sh" --publisher "$publisher"
output=${PISHOO_CALENDAR_DIST:-"$repo/target/calendar-dist"}
mkdir -p "$output"
if [ -n "${PISHOO_PACKAGE_WEBC:-}" ]; then tool=$PISHOO_PACKAGE_WEBC
else
  cd "${TMPDIR:-/tmp}"
  cargo build --locked --manifest-path "$repo/Cargo.toml" -p pishoo --bin pishoo-pack
  tool="$repo/target/debug/pishoo-pack"
fi
"$tool" "$repo/std/libs/calendar/dist/wasmer.toml" -o "$output/calendar-lib.webc" --id "$publisher"
"$tool" "$project/package-dist/wasmer.toml" -o "$output/calendar-app.webc" --id "$publisher"
echo "Two independent Calendar artifacts: $output"

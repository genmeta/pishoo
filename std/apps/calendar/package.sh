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
tool=${GENMETA:-genmeta}
"$tool" package build --no-build "$repo/std/libs/calendar/dist/wasmer.toml" -o "$output/calendar-lib.webc" --id "$publisher"
"$tool" package build --no-build "$project/package-dist/wasmer.toml" -o "$output/calendar-app.webc" --id "$publisher"
echo "Two independent Calendar artifacts: $output"

#!/bin/sh
set -eu
journal_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
tools_dir=$(CDPATH= cd -- "$journal_dir/../../tools" && pwd)
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to the official WASI SDK directory}"
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang"
export CFLAGS_wasm32_wasip2="--target=wasm32-wasip2 -USQLITE_THREADSAFE -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_WAL -DSQLITE_TEMP_STORE=3"
cargo build --locked --release --target wasm32-wasip2 --target-dir "$journal_dir/target" --manifest-path "$journal_dir/Cargo.toml"
mkdir -p "$journal_dir/dist"
cp "$journal_dir/target/wasm32-wasip2/release/journal.wasm" "$journal_dir/dist/journal.component.wasm"
python3 "$tools_dir/package-lib.py" "$journal_dir/dist/journal.component.wasm" "$journal_dir/openapi.json" "$journal_dir/dist/lib.wasm"
wasm-tools validate "$journal_dir/dist/lib.wasm"

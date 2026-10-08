#!/bin/sh
set -eu
note_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH= cd -- "$note_dir/../../.." && pwd)
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to the official WASI SDK directory}"
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang"
export CFLAGS_wasm32_wasip2="--target=wasm32-wasip2 -USQLITE_THREADSAFE -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_WAL -DSQLITE_TEMP_STORE=3"
cargo build --locked --release --target wasm32-wasip2 --manifest-path "$note_dir/Cargo.toml"
mkdir -p "$note_dir/dist"
cp "$note_dir/target/wasm32-wasip2/release/note.wasm" "$note_dir/dist/note.component.wasm"
python3 "$repo/pishoo/tools/package-lib.py" "$note_dir/dist/note.component.wasm" "$note_dir/openapi.json" "$note_dir/dist/lib.wasm"
wasm-tools validate "$note_dir/dist/lib.wasm"

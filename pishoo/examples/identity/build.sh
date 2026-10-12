#!/bin/sh
set -eu
example_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
tools_dir=$(CDPATH= cd -- "$example_dir/../../tools" && pwd)
cargo build --locked --release --target wasm32-wasip2 --target-dir "$example_dir/target" --manifest-path "$example_dir/Cargo.toml"
mkdir -p "$example_dir/dist"
python3 "$tools_dir/package-lib.py" "$example_dir/target/wasm32-wasip2/release/pishoo_identity_example.wasm" "$example_dir/openapi.json" "$example_dir/dist/lib.wasm"
wasm-tools validate "$example_dir/dist/lib.wasm"

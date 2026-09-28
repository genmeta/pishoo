#!/bin/sh
set -eu
repo=$(cd "$(dirname "$0")/../.." && pwd)
manifest="$repo/pishoo/examples/wasm-demo/Cargo.toml"
target="$repo/pishoo/examples/wasm-demo/target/wasm32-unknown-unknown/release"
for name in echo info; do
  cargo build --locked --release --target wasm32-unknown-unknown --manifest-path "$manifest" --package "pishoo-tcp-demo-$name"
  wasm-tools component new "$target/pishoo_tcp_demo_$name.wasm" -o "$target/pishoo_tcp_demo_$name.component.wasm"
  "$repo/pishoo/tools/package-lib.py" "$target/pishoo_tcp_demo_$name.component.wasm" "$repo/pishoo/examples/wasm-demo/$name/openapi.json" "$repo/pishoo/assets/tcp-demo-$name.wasm"
done
"$repo/pishoo/tools/package-lib.py" "$repo/pishoo/tests/fixtures/wasi-http-read-request-then-respond.wasm" "$repo/pishoo/examples/wasm-demo/trailers/openapi.json" "$repo/pishoo/assets/tcp-demo-trailers.wasm"

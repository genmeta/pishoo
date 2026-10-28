# Stream lifecycle probe

A real WASI P2 component importing the Lib-local event interface. It waits without
polling and writes only on notification or the bounded heartbeat. `/notify`
wakes streams in the same Lib. No network, filesystem or identity access.

From this directory: `cargo build --locked --release --target wasm32-wasip2`.
Copy `target/wasm32-wasip2/release/lib_stream_probe.wasm` to
`../pishoo-stream-probe.wasm` after changing the source. The native tests use the
checked-in component, so they do not need a guest compiler.

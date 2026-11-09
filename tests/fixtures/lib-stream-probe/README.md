# Standard HTTP stream lifecycle probe

A real WASI P2 component using only standard HTTP and monotonic-clock imports.
It has no Message, database, identity, CLI, or events dependency. `/sse`, `/ndjson`
and `/text` exercise different response content types with periodic bytes;
`/binary?count=16` exercises bounded output backpressure. `/idle` waits on a
60-second standard timer, `/eof` closes its body then waits, and `/trap` fails
after sending headers. `/budget?count=20` combines real one-second waits and
bounded computation to exercise replenishment beyond the CLI deadline. The optional `count` query bounds periodic output.

From this directory: `cargo build --locked --release --target wasm32-wasip2`.
Copy `target/wasm32-wasip2/release/lib_stream_probe.wasm` to
`../pishoo-stream-probe.wasm` after changing the source. Native tests use the
checked-in component, so they do not require a guest compiler.

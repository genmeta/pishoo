# Host capability acceptance component

Exports standard WASI HTTP incoming-handler and CLI run. Imports the SDK's
request-identity@0.2.0 tuple getters and standard WASI outgoing HTTP. The HTTP
handler reports identity or calls /std/native; CLI run checks caller=None and
writes the native HTTP result to its private /db.

Rebuild from outside the parent integration Cargo configuration:

```sh
cd /private/tmp
cargo build --offline --locked --manifest-path /absolute/path/to/pishoo/pishoo/tests/fixtures/host-client/Cargo.toml --target wasm32-wasip2 --release
cp /absolute/path/to/pishoo/pishoo/tests/fixtures/host-client/target/wasm32-wasip2/release/pishoo_host_client.wasm /absolute/path/to/pishoo/pishoo/tests/fixtures/pishoo-host-client.wasm
```

`host_client` unit tests execute the checked-in component, including local
streaming and bounded background runs. `host_client_real_dhttp_transport` is
ignored by default; run it alone with `--ignored` because it sets global
TLS/DNS/home resources and requires OpenSSL and loopback UDP.

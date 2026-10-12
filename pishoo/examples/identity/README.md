# Identity SDK example

This minimal WASI HTTP component calls `pishoo_sdk::request_identity` directly.
It exports the standard WASI HTTP handler and returns JSON with `local_name`,
`caller_name` and `caller_is_owner`; anonymous callers have a null caller name and
false owner status. The Pishoo daccess rules decide who may invoke the route.

Install Rust with the `wasm32-wasip2` target, Python 3 and `wasm-tools`, then run
from this directory:

```sh
rustup target add wasm32-wasip2
sh build.sh
pishoo lib check dist/lib.wasm
pishoo --id YOUR_IDENTITY lib install identity dist/lib.wasm
```

Restart Pishoo after installation. With a DHTTP-capable client, request
`https://YOUR_IDENTITY/std/api/identity/`. Grant access through daccess if needed;
the OpenAPI manifest does not grant permission. HEAD returns headers only.

The repository example depends on `../../../wit`. The developer bundle rewrites
this to `../../sdk`, so it builds independently of the Pishoo checkout. For a new
Lib, copy this example, update its Cargo name/OpenAPI and the output filename in
`build.sh`. The SDK is not yet published to crates.io.

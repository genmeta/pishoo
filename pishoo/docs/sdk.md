# Pishoo WASM developer SDK

The SDK directly exports the existing WIT guest bindings:

```rust
use pishoo_sdk::{request_identity, signatures};

let local = request_identity::local_name();
let caller = request_identity::caller_name();
let owner = request_identity::caller_is_owner();
```

`request_identity` provides the local name, optional verified caller name and
owner check. `signatures` provides `sign`, `verify`, `SignError` and `VerifyError`.
See `sdk/README.md` in the developer bundle for signature usage and limits.
These are WASM imports implemented by Pishoo. WASI HTTP request/response types and
the HTTP handler export use the standard `wasip2` crate.

## Create the distribution

In the Pishoo checkout, with Python 3.11+ and Rust/Cargo installed:

```sh
python3 pishoo/tools/package-sdk.py
# With dependencies already cached:
python3 pishoo/tools/package-sdk.py --offline
```

The script produces `target/sdk-dist/pishoo-sdk-0.1.0.crate` and
`target/sdk-dist/pishoo-sdk-0.1.0.tar.gz`; the version comes from the SDK manifest.
Use `--output-dir PATH` to select another destination. Cargo verifies the `.crate`
in isolation. The script does not publish to crates.io or install any Libs.

The developer archive contains:

```text
pishoo-sdk-0.1.0/
  sdk/                  Rust crate, WIT, Cargo.lock, documentation and license
  examples/identity/    Minimal HTTP component using the SDK
  examples/journal/     Journal component, embedded page and SQLite
  tools/package-lib.py  Attach an OpenAPI manifest to a component
  README.md
  LICENSE
```

Generated targets, local credentials and prebuilt Libs are not included. Both
examples depend on `../../sdk` inside the archive; the repository versions use
`../../../wit`. WIT remains a single source under the repository SDK crate's
`wit/pishoo-identity` directory, shared with the host. SDK 0.1.0 imports identity
interface version 0.1.0; use a host that implements request-identity and signatures.

## Build the minimal example

Install `wasm-tools`, then unpack the developer archive and run:

```sh
tar -xzf pishoo-sdk-0.1.0.tar.gz
cd pishoo-sdk-0.1.0
rustup target add wasm32-wasip2
sh examples/identity/build.sh
pishoo lib check examples/identity/dist/lib.wasm
pishoo --id YOUR_IDENTITY lib install identity examples/identity/dist/lib.wasm
```

Restart Pishoo to load the installed Lib. Request
`https://YOUR_IDENTITY/std/api/identity/` through a DHTTP-capable client.
The response is JSON:

```json
{
  "local_name": "alice.dhttp.net",
  "caller_name": "bob.dhttp.net",
  "caller_is_owner": false
}
```

Anonymous callers have `caller_name: null` and `caller_is_owner: false`. The
Pishoo daccess rules still authorize each method/path; the manifest and SDK do not
grant access. Identity comes from the verified handshake, never HTTP headers.
Lib outgoing HTTP requests remain denied.

## Build Journal or your own Lib

Journal uses SQLite and additionally requires the official WASI SDK:

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh examples/journal/build.sh
pishoo lib check examples/journal/dist/lib.wasm
```

Use `examples/identity` as the starting point for a new Lib. Preserve the standard
WASI HTTP handler export, update the Cargo name and OpenAPI paths, and update the
build script's WASM output filename. Keep the SDK dependency:

```toml
[dependencies]
pishoo-sdk = { version = "=0.1.0", path = "../../sdk" }
wasip2 = "=1.0.4"
```

SDK 0.1.0 is not yet published to crates.io. Resolve the path relative to your own
`Cargo.toml`. A Lib built for `wasm32-wasip2` is already a component; the included
`package-lib.py` attaches its OpenAPI 3.1 manifest and `wasm-tools validate` checks
the resulting `lib.wasm`. No custom HTTP or identity wrapper is required.

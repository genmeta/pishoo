# Pishoo WASM developer SDK

For the selected WEBC package layout, independent frontend and metadata OpenAPI,
see the [Pishoo WASM developer guide](developing-wasm.md). That guide describes
the target distribution contract and identifies the remaining host/store work;
the SDK below supplies the existing guest bindings.

The current SDK exposes verified identity as a tuple:

```rust
use pishoo_sdk::{identity, signatures};

let local = identity::local(); // (name, owner_hash)
let caller = identity::caller(); // Option<(name, owner_hash)>
let owner = caller.as_ref() == Some(&local);
```

`identity` uses request-identity@0.2.0. Both tuple elements must match for an
owner operation. Anonymous and background invocations have no caller; invalid
SKI on a named identity traps. `signatures` retains @0.1.0; verification takes
`local.0` or a verified caller name. The legacy `request_identity` export and
its @0.1.0 functions remain available for installed components.

WASI HTTP request/response types and incoming/outgoing handler interfaces are
standard. Ordinary installed Libs use the same Endpoint/native API grants;
local calls keep their real caller and remain authorized. Bounded standard CLI run is described in
[host capabilities](host-capabilities.md).

## Create the distribution

In the Pishoo checkout, with Python 3.11+ and Rust/Cargo installed:

```sh
python3 pishoo/tools/package-sdk.py
# With dependencies already cached:
python3 pishoo/tools/package-sdk.py --offline
```

The script produces `target/sdk-dist/pishoo-sdk-0.1.0-preview.1.crate` and
`target/sdk-dist/pishoo-sdk-0.1.0-preview.1.tar.gz`; the version comes from the SDK manifest.
Use `--output-dir PATH` to select another destination. Cargo verifies the `.crate`
in isolation. The script does not publish to crates.io or install any Libs.

The developer archive contains:

```text
pishoo-sdk-0.1.0-preview.1/
  sdk/                  Rust crate, WIT, Cargo.lock, documentation and license
  examples/journal/     Journal backend, public frontend, wasmer.toml and SQLite
  tools/package-lib.py  Attach an OpenAPI manifest to a component
  developing-wasm.md    WASI HTTP and the selected WEBC distribution design
  host-capabilities.md  Verified identity and host-granted execution
  README.md
  LICENSE
```

Generated targets, local credentials and prebuilt Libs are not included. Journal
depends on `../../sdk` inside the archive; the repository version uses
`../../../wit`. WIT remains a single source under the repository SDK crate's
`wit/pishoo-identity` and `wit/pishoo-identity-v2` directories, shared with the host.
Use a host implementing the identity interface imported by your component.

## Start a new Lib

Unpack the developer archive and install the component target:

```sh
tar -xzf pishoo-sdk-0.1.0-preview.1.tar.gz
cd pishoo-sdk-0.1.0-preview.1
rustup target add wasm32-wasip2
```

Follow the minimal HTTP example in the [WASM developer guide](developing-wasm.md)
to create a backend, independent OpenAPI and public frontend. Sign a WEBC release
with `package-webc`, then install it through the Workspace or CLI. Online
installation activates the app immediately; offline CLI changes load on startup.
The Pishoo daccess rules authorize each method/path; the manifest and SDK do not
grant access. Identity comes from the verified handshake, never HTTP headers.

## Build Journal or your own Lib

Journal uses SQLite and additionally requires the official WASI SDK:

```sh
WASI_SDK_PATH=/path/to/wasi-sdk PISHOO_PACKAGE_WEBC=/path/to/package-webc \
  sh examples/journal/build.sh --publisher alice.smith.dhttp.net
pishoo lib check examples/journal/dist/journal-0.1.0.webc
```

Journal ships one signed WEBC release containing a standalone WASI HTTP backend,
`public/index.html`, JS/CSS assets, and one metadata OpenAPI. The build script
checks Cargo, wasmer.toml and OpenAPI versions, then uses `package-webc` to sign
with the publisher’s local DHTTP credentials. Run without `--publisher` to build
only the unsigned backend; use the signed `.webc` for host installation. See
`examples/journal/README.md` for static access groups and release instructions.

Use the developer guide or `examples/journal` as the starting point for a new Lib. Preserve the standard
WASI HTTP handler export, update the Cargo name and OpenAPI paths, and update the
build script's WASM output filename. Keep the SDK dependency:

```toml
[dependencies]
pishoo-sdk = { version = "=0.1.0-preview.1", path = "../../sdk" }
wasip2 = "=1.0.4"
```

SDK 0.1.0 is not yet published to crates.io. Resolve the path relative to your own
`Cargo.toml`. A Lib built for `wasm32-wasip2` is already a component; the included
`package-lib.py` attaches its OpenAPI 3.1 manifest and `wasm-tools validate` checks
the resulting legacy `lib.wasm`; that file is a binding example, not a current installable release. New installations use a signed `.webc` from `package-webc`. No custom HTTP or identity wrapper is required.

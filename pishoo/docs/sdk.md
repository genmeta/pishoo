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

`identity` uses request-identity@0.1.0. Both tuple elements must match for an
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

The script produces `target/sdk-dist/pishoo-sdk-0.1.0-preview.2.crate` and
`target/sdk-dist/pishoo-sdk-0.1.0-preview.2.tar.gz`; the version comes from the SDK manifest.
Use `--output-dir PATH` to select another destination. Cargo verifies the `.crate`
in isolation. The script does not publish to crates.io or install any Libs.

The developer archive contains:

```text
pishoo-sdk-0.1.0-preview.2/
  sdk/                  Rust crate, WIT, Cargo.lock, documentation and license
  tools/package-lib.py  Attach an OpenAPI manifest to a component
  developing-wasm.md    WASI HTTP and the selected WEBC distribution design
  host-capabilities.md  Verified identity and host-granted execution
  README.md
  LICENSE
```

Generated targets, local credentials, business App/Lib sources and prebuilt Libs are not included. WIT remains a single source under the repository SDK crate's
`wit/pishoo-identity` and `wit/pishoo-identity-v2` directories, shared with the host.
Use a host implementing the identity interface imported by your component.
Business products, including Journal, are maintained in [independent repositories](business-repositories.md).

## Start a new Lib

Unpack the developer archive and install the component target:

```sh
tar -xzf pishoo-sdk-0.1.0-preview.2.tar.gz
cd pishoo-sdk-0.1.0-preview.2
rustup target add wasm32-wasip2
```

Follow the minimal HTTP example in the [WASM developer guide](developing-wasm.md)
to create a backend, independent OpenAPI and public frontend. Sign a WEBC release
with `genmeta build`, then install it through the Workspace or CLI. Online
installation activates the app immediately; offline CLI changes load on startup.
The Pishoo daccess rules authorize each method/path; the manifest and SDK do not
grant access. Identity comes from the verified handshake, never HTTP headers.

## Use the SDK in an independent Lib

Journal and Calendar keep their App and Lib in their own product repositories.
See [repository boundaries](business-repositories.md) for locations and acceptance-test inputs.
The SDK is not yet published to crates.io. With sibling Pishoo and Lib checkouts:

```toml
[dependencies]
pishoo-sdk = { version = "=0.1.0-preview.2", path = "../pishoo/wit" }
wasip2 = "=1.0.4"
```

For an extracted developer archive, point the dependency at its `sdk/` directory.
A Lib built for `wasm32-wasip2` is already a component. Validate it using
`wasm-tools validate`, then use `genmeta build --no-build` to sign the WEBC package.
The included `tools/package-lib.py` attaches the OpenAPI document to the component;
signatures belong to the outer WEBC package. SDK packaging does not build or bundle business products.

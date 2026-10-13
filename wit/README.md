# Pishoo Rust SDK

`pishoo-sdk` exports `identity` for the two-function
`pishoo:identity/request-identity@0.2.0` interface. The host reads verified
identity facts directly; the tuple is `(name, owner_hash from certificate SKI)`.
The old `request_identity` export (@0.1.0) remains available for existing Libs.
`signatures` keeps its @0.1.0 interface.

```rust
use pishoo_sdk::identity;

let local: (String, String) = identity::local();
let caller: Option<(String, String)> = identity::caller();
let owner = caller.as_ref() == Some(&local);
```

Both name and hash must match. Anonymous HTTP requests and background runs return
`None` for caller. Invalid SKI on a named identity traps rather than returning an
empty hash. Outgoing calls do not change these invocation facts.

The SDK is currently distributed as source and a Cargo `.crate` archive; it has
not been published to crates.io. In an extracted developer bundle, a Lib under
`examples/<name>` uses:

```toml
[dependencies]
pishoo-sdk = { version = "=0.1.0-preview.1", path = "../../sdk" }
wasip2 = "=1.0.4"
```

```rust
use pishoo_sdk::{request_identity, signatures};

let local: String = request_identity::local_name();
let caller: Option<String> = request_identity::caller_name();
let owner: bool = request_identity::caller_is_owner();

let data = b"hello";
let signature = signatures::sign(data)?;
let valid = signatures::verify(&signature, data, &local)?;
```

The signing example assumes a function returning an appropriate `Result`.
`caller_name()` returns `None` for anonymous callers. `caller_is_owner()` requires
both the normalized name and certificate owner hash to match. Signature inputs
are limited to 1 MiB, signatures to 8 KiB; verification uses only the current local
or verified caller identity. Other identities return `VerifyError::Unavailable`.

Build HTTP components for `wasm32-wasip2`, exporting the standard
`wasip2::exports::http::incoming_handler::Guest` handler with
`wasip2::http::proxy::export!`. The SDK identity functions are WASM imports supplied
by Pishoo. They cannot run as a native client or authenticate a caller themselves.
WASI HTTP outgoing requests remain denied, and daccess still authorizes each
complete HTTP method/path. Use a Pishoo host with the request-identity interface.

The developer bundle contains a minimal `examples/identity` component, the full
`examples/journal` component and `tools/package-lib.py`. Both examples include
locked dependencies, OpenAPI manifests and build scripts. Installing/updating a
Lib requires restarting Pishoo; neither example changes access rules.

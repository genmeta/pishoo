# Genmeta package commands

`genmeta package` is the common entry point for App and Lib packages. It replaces
`genmeta install`, `genmeta pack`, and `genmeta upload`; there are no top-level
compatibility aliases. App/Lib selection uses `--type`, while signed metadata and
HTTP JSON retain the existing `kind` field.

```sh
genmeta package build ./std/libs/profile -o ./profile.webc --id dev.dhttp.net
genmeta package publish ./profile.webc
genmeta package install std.dhttp.net/workspace --id sky.lee.dhttp.net
genmeta package update workspace --version 0.2.0 --id sky.lee.dhttp.net
genmeta package list --id sky.lee.dhttp.net
genmeta package info message --type lib --id sky.lee.dhttp.net
genmeta package remove alice.smith.dhttp.net/message --type lib --id sky.lee.dhttp.net
```

`install`, `update`, `remove`, and `info` default to App. `list` returns both types
unless `--type app|lib` is given. Management commands use the selected identity's
running Pishoo service and its installed database catalog; they never scan the
local filesystem or query the store for `list`, `info`, or `remove`.

## Building and publishing

`build` executes a local recipe before signing. The source `wasmer.toml` contains:

```toml
[build]
command = ["sh", "build.sh"]
manifest = "dist/wasmer.toml"
```

The command is an argument array, executed in the source manifest's directory;
no shell string is assembled. A shell runs only when explicitly selected in the
recipe. `manifest` is the generated package manifest, relative to that directory;
when omitted, the original manifest is used. It must remain within the project.
The recipe is local build input and is not copied into signed metadata.

`build` runs the recipe on every invocation and stops if it fails. It then signs
and checks the generated package through the publisher identity’s running Pishoo validation API.
Build output goes to stderr; stdout contains the final package JSON. Existing
output files are not overwritten. `--id` checks the built package publisher;
it does not rewrite it or its dependencies. Standard projects use their existing
`build.sh`; WASI SDK, Rust/WASM tools, and Bun requirements remain unchanged.
For these scripts `PISHOO_WORKSPACE_PUBLISHER` selects the build publisher.

For already-built input, use `genmeta package build --no-build PATH -o FILE.webc` directly. It does not
run the recipe with `--no-build`. `publish` accepts a signed WEBC file and uses the
existing pinned store publishing protocol; building never publishes automatically.

## Installation, update and removal

The CLI checks the installed catalog before changing a package:

- `install`: resolve the selected store release; the same installed ID/version
  returns `unchanged`. A different installed version requires `update`.
- `update`: require an installed package and resolve its original publisher's
  release, using the latest published version unless `--version` is supplied.
- A different publisher with the same short name is a conflict. Remove the old
  package explicitly before installing the new publisher's package.
- `remove`: resolve the installed full ID and validate a supplied publisher.
  Keep business data, authorization rules, and dependency packages. A dependent
  App or Lib blocks removal, and the host reports its type and full ID.

These CLI preflight checks reuse `POST /std/sys/package/store-install` for the
actual installation/update. This API retains its existing upsert behavior and
per-package commit semantics; preflight is not a server-side compare-and-swap.
Concurrent administrative changes can invalidate the preview. Exact dependency,
owner-hash and publisher checks still run at commit. Successful dependencies
remain if a later package fails. There is no forced removal, automatic dependency
cleanup, automatic retry, or transaction spanning packages.

## Host API changes

Collection/detail paths remain `GET /std/sys/package/apps[/<name>]` and
`GET /std/sys/package/libs[/<name>]`; deletion uses the same detail path with
`DELETE`. URLs continue to use short names.

A successful detail GET now returns `Package-Id: <full installed ID>`. The new
CLI requires this capability before removal and sends
`Expected-Package-Id: <full installed ID>` on DELETE. The host resolves the name
again, rejects a mismatch or disappearance with 409, and removes only that exact
ID. It never falls back to deleting a replacement publisher. Older callers may
omit the header and retain their existing behavior. The header is rejected for
other methods. Both endpoints retain owner authorization and `Cache-Control:
no-store`.

No Pishoo state fields, schema, or cross-module function signatures change.

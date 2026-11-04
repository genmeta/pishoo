# Local development with Jade

`jade run` uses the explicitly selected development identity and reuses the running
Pishoo process. Pass the actual child name, for example `--id dev.alex.chen`;
Jade never adds or removes `dev.` from explicit input. A main name `dev.wang` requires
`--id dev.dev.wang`. Without `--id`, Jade selects the `dev` child of the configured default
identity: a default of `dev.wang` selects `dev.dev.wang`, while an existing
child such as `dev.dev.wang` or `dev.alex.chen` stays unchanged. Without a configured
default Jade uses `dev.zero.ai`. Install
ordinary identity credentials for that name beforehand; no developer certificate
or subscription is required. Jade enables its normal listener through `pishoo configure`, never starts
another daemon, and leaves the user's default identity unchanged.

```sh
jade run
jade run --id dev.zero.ai
# App: https://dev.zero.ai.dhttp.net/app/<name>/
# Standard API: https://dev.zero.ai.dhttp.net/std/<name>/...
# Ordinary API: https://dev.zero.ai.dhttp.net/<name>/...
```

## Dependencies

```toml
[[dependencies]]
author = "std.dhttp.net"
name = "profile"
version = "0.3.0"
path = "../profile"

[[dependencies]]
author = "std.dhttp.net"
name = "message"
version = "0.2.1"
```

A dependency with `path` is built and watched locally. Paths resolve relative to
the declaring `jade.toml`, including nested dependencies. Local package author,
name and version must match the dependency. Cycles, conflicting versions/authors,
and different local projects with the same short name are rejected. Dependencies
without `path` are installed at exactly the declared version through the normal
Pishoo store API, retaining its signature, receipt, revocation and recursive
version checks. Unchanged dependency coordinates are reused during watching.
`path` is development-only and omitted from release metadata. `jade build`
creates an unsigned WAZ without developer credentials. Only
`jade publish` requires developer credentials, signs the prebuilt snapshot and
uploads it. Formal store installation continues to require a signed release.

## Files and data

```text
<dev-identity>/dev/<name>/manifest.json
<dev-identity>/dev/<name>/lib.wasm
<dev-identity>/dev/<name>/app/index.html
<dev-identity>/.volume/<name>/              # ordinary guest /db
<dev-identity>/.volume/std/<name>/          # standard guest /db
```

Pishoo selects local development files independently of the identity name.
Normal application URLs select these files on the chosen identity; there is no `/dev` URL alias. Standard guests
receive `/<name>/...` after `/std` is removed; ordinary guests receive the suffix
after their name. Native services retain precedence. Installed dependencies keep
their normal URLs and authorization. Local files require the authenticated real
owner, even when ACL rules allow visitors. Jade selects a development child with its own
native databases and business data, separate from the base identity.

Jade builds all local projects before publishing. Each successful project is
atomically swapped into `dev/<name>` under a directory lock. Builds are serial;
changes during a build trigger another build before publishing. Failed builds
leave previous files available. Pishoo opens App files on request and lazily
compiles WASM, caching by full package identity and file metadata. Invalid or
removed WASM cannot execute a stale cache entry. Already-open files and captured
invocations may finish.

Ctrl+C/SIGTERM removes only the local App/API directories still owned by that
watcher, including local dependencies. It preserves all development data and
installed store dependencies. `--fresh` explicitly archives each local project's
previous data before starting empty. A later publisher's replacement is preserved.
SIGKILL/power loss cannot clean up; explicitly remove leftover `dev/<name>`
directories. Old `dev-data` files are not moved or deleted automatically.

Filesystem publication rejects symlinks, foreign ownership and other-user writable
directories. Different projects publishing the same development identity/name
share that slot and data. Normal installed files are retained underneath a local
replacement and become available again when the local slot is removed.

## Verification

```sh
cargo test --lib development::tests
JADE_TEST_BINARY=/path/to/jade PISHOO_TEST_BINARY=/path/to/pishoo \
  cargo test --lib development::tests -- --include-ignored
```

The cross-repository fixture checks publication and signal cleanup with isolated
profiles; unit tests cover normal URLs, owner access, dependency fallback, identity
isolation, cache invalidation, real WASM execution and persistent data.

Formal installation verifies the trusted certificate chain, codeSigning usage,
package signature and exact author identity. Pishoo does not require a `dev.`
prefix in the signing name; developer naming and subscription binding belong to
Jade and CertServer.

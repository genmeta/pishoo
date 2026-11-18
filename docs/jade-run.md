# Local development with Jade

Dependencies contain only an exact `name` and `version`; local Jade declarations
may also set `path`, which is omitted from the release. The store resolves each
package name to its unique author. A name stays reserved after withdrawal.
Cross-author name claims return `409 package_name_conflict`; every repeated
version upload, including identical bytes, returns `409 package_version_conflict`.
Historical signed dependencies with authors remain readable without rewriting packages.

`jade run` uses `package.author` when configured. Otherwise it selects the `dev`
child of the configured default identity, retaining an existing development child.
Without either setting Jade reports `E_IDENTITY_REQUIRED`. Install ordinary identity
credentials for that name beforehand. Jade enables its normal listener through
`pishoo configure` and reuses the running Pishoo process.

```sh
jade run
# App: https://dev.zero.ai.dhttp.net/app/<name>/
# Standard API: https://dev.zero.ai.dhttp.net/std<declared-path>
# Ordinary API: https://dev.zero.ai.dhttp.net/<name>/...
```

## Dependencies

```toml
[[dependencies]]
name = "profile"
version = "0.3.0"
path = "../profile"

[[dependencies]]
name = "message"
version = "0.2.1"
```

A dependency with `path` is built and watched locally. Paths resolve relative to
the declaring `jade.toml`, including nested dependencies. Local package name and
version must match the dependency. Cycles, conflicting versions,
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
<identity>/
├── lib/
│   └── std/comm/
│       ├── group.wasm.1.0.0       # backend version 1.0.0
│       ├── group.wasm.2.0.0       # backend version 2.0.0
│       └── group.preview/         # current preview ownership and originals
│           ├── manifest.json
│           ├── state.json
│           ├── previous-lib
│           └── previous-app/
├── app/std/comm/
│   ├── group -> group.1.0.0
│   ├── group.1.0.0/
│   └── group.2.0.0/
└── .volume/std/comm/group/         # every version mounts this directory as /db
```

Installed releases and Jade previews use the same runtime locations on their
selected identity. `dev` identifies the development identity, such as `dev.zero.ai`.
The WASM filename uses the project's actual `package.version`. Complete package
name segments form module directories, with the version on the final backend
filename. The active frontend link selects its version directory.
`std/comm/group` has App URL `/app/std/comm/group/`. Previous `%2F` business data directories
migrate when the complete-path destination is absent.

The companion `lib/<name>.preview/` directory identifies local files and retains
resources displaced by the preview. Host startup reads the retained registered
release files, then selects local resources for authenticated owner requests.

Pishoo selects local development files independently of the identity name.
Normal application URLs select these files on the chosen identity; there is no `/dev` URL alias. Standard guests
declare their business paths in the embedded OpenAPI. Pishoo selects the matching
standard Lib by its exact declared path and method, then removes `/std` before
execution. Package names identify artifacts and data directories independently of
these paths. Ordinary guests receive the suffix after their name. Native services
retain precedence. Installed dependencies keep
their normal URLs and authorization. Local files require the authenticated real
owner, even when ACL rules allow visitors. Jade selects a development child with its own
native databases and business data, separate from the base identity.

Standard previews refresh local Libs through the existing loader and its
file-metadata cache, then use the same API router as installed standard packages.
Source replacement retires the cached Lib, and removing
the local WASM restores the installed package. A preview overrides its exact author/name and project version. Other installed
versions retain their own resources and dependency bindings. Different packages claiming the same path return 409.
Undeclared standard paths return 404 and unsupported methods return 405.

Jade builds all local projects before publishing. Each successful project is
published to the identity's `lib/` and `app/` under its directory lock, together
with an atomically replaced `lib/<name>.preview/` record. Failed file publication
rolls back completed moves. Builds are serial;
changes during a build trigger another build before publishing. Failed builds
leave previous files available. Pishoo opens App files on request and lazily
compiles WASM, caching by full package identity and file metadata. Invalid or
removed WASM cannot execute a stale cache entry. Already-open files and captured
invocations may finish.

Local App files support the host's conditional requests and single byte ranges,
but retain `Cache-Control: no-store`, including conditional responses, redirects
and errors. Installed App caching does not override a local replacement. See
[static file HTTP behavior](static-files.md).

Ctrl+C/SIGTERM removes the App/API files and companion records still owned by that
watcher, including local dependencies, and restores displaced resources. It preserves all development data and
installed store dependencies. `--fresh` explicitly archives each local project's
previous data before starting empty. A later publisher's replacement is preserved. A running watcher checks its current
resource ownership before each publication. After an installer or another watcher
takes over, automatic publication reports `E_DEV_REPLACED`; a new `jade run` starts
a new preview.
After SIGKILL or power loss, rerunning Jade reconciles a completed preview record
before publishing again. An interruption during file movement can leave `.jade-*`
staging directories in the identity root, containing retained resources for explicit
recovery. Earlier `dev/<name>` and `dev-data` directories remain historical files.

WASM files and frontend version directories use restricted ordinary-file traversal.
The active App link accepts the corresponding version directory in the same parent.
Publication verifies ownership and directory permissions. Different projects publishing the same development identity/name
share that slot and data. Installed resources displaced by a local replacement are retained in its companion
directory and restored when that watcher stops. A later formal installation owns
its replacement files, which remain available when Jade cleans up.

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

## Exact dependency versions

Installed versions use `lib/<name>.wasm.<version>`. The host resolves a local WASI
HTTP call through the executing Lib's exact dependency snapshot. Browser requests
can select a release with `Pishoo-Package-Version`; responses report the executing
version in the same field. Default calls use the registered active version.
Versions of one package share its business volume. Schema and data compatibility
are the business package's responsibility.

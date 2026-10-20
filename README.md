<p align="center">
  <img src="https://media.dhttp.net/img/pishoo/pishoo-readme-title.jpg" alt="PISHOO — It's the gateway that keeps your data private and secure." width="900">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-0.9.0--preview.2-1f6feb?style=flat-square" alt="Version 0.9.0-preview.2">
  <img src="https://img.shields.io/badge/Rust-2024-dea584?style=flat-square&logo=rust&logoColor=white" alt="Rust 2024">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-4c956c?style=flat-square" alt="Apache-2.0 license"></a>
</p>

Pishoo takes its name from Pixiu (貔貅), an auspicious creature in ancient Chinese mythology. The name reflects its role in protecting local data, securing the network boundary, and controlling external access. Like Nginx in the conventional HTTP stack, Pishoo provides web serving, reverse proxying, load balancing, and support for NAT traversal in the DHttp stack. The key difference is how services are exposed. A conventional Nginx deployment typically exposes a service through a fixed listening port on a publicly reachable address. With DHttp, the service itself does not need a fixed public IP address or port, allowing Pishoo to be deployed on any endpoint.

## Why Pishoo?

**Gateways Protect Data Assets:** Why does your private data often seem more exposed than the data held by large platforms? One reason is that large platforms place gateways in front of their services. Like a gate that protects private territory, a gateway helps control what a service exposes and who can access it. Put Pishoo in front of the data and services you run yourself to retain that control.

- **Personal Server:** Cloud servers are not your only option. With Pishoo, you are free to choose any device as your personal server.
- **Agent Home:** The best way to access your AI agent is directly in your browser, not through a chat app. To make that possible, run your agent behind Pishoo.

## How it works

- **DHttp Inside:** Pishoo is a DHttp-native gateway that lets any endpoint expose services without requiring a public IP address or a fixed listening port.
- **APIs Everywhere:** The agentic internet is driving a new wave of API openness, in which every agent ultimately has its own open API.
- **Access Control:** Open APIs do not mean unrestricted access. Pishoo grants API access to authorized names rather than requiring a traditional login.

## Getting started

Pishoo 0.9 uses per-identity SQLite configuration, name-based access control,
and WASM components. See the [configuration API](pishoo/docs/config-api.md),
[management guide](pishoo/docs/management.md) and [system paths](pishoo/docs/system-paths.md).

### Local development build

The 0.9.0-preview.2 release preparation uses compatible crates.io versions of DHTTP, DQUIC, DDNS, H3X and daccess. Publication order and lockfile preparation are listed in [RELEASING.md](RELEASING.md); no sibling checkouts are required. Use Rust 1.97.1, Bun 1.4.2 and Node.js 23.11.0 (the tested tool versions) on PATH, plus a native C/C++ build toolchain, then run:

```sh
git clone --branch feat/release-pishoo-0.9.0 https://github.com/genmeta/pishoo.git
cd pishoo
cargo build --locked -p pishoo --bin pishoo
cargo test --locked --workspace -- --test-threads=1
cargo check --locked --workspace --all-targets
```

The host build is independent of frontend tooling. The [standard Apps and Libs](std/README.md) live separately from the host source: Workspace is an ordinary user App with separate Profile and Message WASM Libs, exact backend dependencies, normal install/uninstall behavior and private data. Build the three independently with [the Workspace scripts](std/apps/workspace/README.md); publishing new embedded-OpenAPI Libs still requires a compatible store, as noted below. Host release packages contain no business App/Lib artifacts; install Workspace explicitly after starting Pishoo, as shown below. First-time host builds need access to GitHub and crates.io. Run `./target/debug/pishoo` with credentials in your account’s `~/.dhttp`. Release versions and pending registry lockfile preparation are listed in [RELEASING.md](RELEASING.md). Ancestor or user-level Cargo path patches can override registry dependencies; use a checkout outside those configured directories when reproducing it.

### Install Pishoo

Pishoo supports mainstream Linux distributions and macOS on both Arm and x86 architectures. Install the `gmutils` operations toolkit alongside Pishoo to manage identities and install Apps from the store. The two host packages can be installed in either order; Pishoo has no runtime dependency on gmutils.

#### Linux (Debian 11+)

```sh
sudo install -d -m 0755 /etc/apt/keyrings
wget -qO- https://download.dhttp.net/ppa/key/public.key | gpg --dearmor | sudo tee /etc/apt/keyrings/genmeta.gpg > /dev/null

sudo tee /etc/apt/sources.list.d/genmeta.sources > /dev/null <<'EOF'
Types: deb
URIs: https://download.dhttp.net/ppa/genmeta
Suites: stable preview
Components: main
Signed-By: /etc/apt/keyrings/genmeta.gpg
EOF

sudo apt update
sudo apt install pishoo gmutils
```

The Linux package runs `pishoo.service` as root. It loads the DHTTP global home
(default `/etc/dhttp`) and `.dhttp` under each account's actual home from the
system account database. This supports `/root`, `/home`, `/var/home` and custom
home locations across distributions. Account discovery uses `getent passwd`
(NSS), with `/etc/passwd` as the fallback on systems without `getent`.

Each identity keeps its configuration, credentials and data in its original
directory. New accounts and identities are discovered while the service runs.
Duplicate normalized identity names are rejected with both paths in the diagnostic.
Upgrades remove the previous unmodified package-generated single-user override;
administrator unit files and customized overrides are preserved. Pishoo no longer
uses `DHTTP_HOME`. No `dhttp` group membership is required.

#### macOS

```sh
brew tap genmeta/preview https://github.com/genmeta/homebrew-preview
brew trust genmeta/preview
brew update
brew install pishoo gmutils
```

### Create an identity

You can purchase a name and certificate, then place them in the appropriate location. However, we recommend using `gmutils` to install them automatically.

```sh
genmeta identity apply
```

### Install Workspace from the store

Install a gmutils version that provides `genmeta install`, configure your identity,
then check or start the Pishoo service:

```sh
# Debian/Ubuntu: the package normally enables and starts the system service.
systemctl status pishoo

# macOS (current user's service)
brew services start pishoo
```

On Debian/Ubuntu, manual startup is only needed if the service is disabled or
installation-time startup was prevented by system policy. Use
`sudo systemctl enable --now pishoo` when you want to enable and start it.

Once Pishoo has loaded the identity and is reachable, run the following **as the
account that owns the identity**, replacing `alice.smith` with its name:

```sh
genmeta install workspace --id alice.smith
```

The command calls the running identity's Pishoo installation API. Pishoo downloads
and verifies the published Workspace App and installs its exact Profile and Message
Lib dependencies. The App is available at `https://<identity>/workspace/`
immediately after installation. The selected App and dependency versions must
already be published to a compatible store; this command does not publish them.
Installation and updates use one JSON `PUT /std/sys/app?name=workspace` request
with exactly `publisher` and `version`; there is no local-package upload fallback.
The unchanged store currently rejects newly built Libs with embedded OpenAPI,
so those releases must wait for store compatibility before installation.

Pishoo package installation and startup initialize native services and databases
without downloading business Apps or invoking gmutils. Store unavailability does
not prevent host installation or startup. Installing an App is an explicit step
for each identity; it preserves the normal ACL and dependency rules.

### Current development configuration

Linux root runs load the DHTTP global home and system accounts' actual `~/.dhttp` directories. Other runs use the current user's `~/.dhttp`. Identities without `db/config.db` are skipped at startup and during discovery; credentials alone do not enable hosting. Each Server reads its own `<identity-home>/<identity>/db/config.db`; there is no instance configuration file or database. Schema v1 has one `settings(listen, enabled)` row and a `proxy_locations(location, proxy_pass)` table. Proxy targets are local HTTP/TCP services, for example `127.0.0.1:8080` or `http://127.0.0.1:8080/api/`. Pishoo installs System DNS, anonymous H3 DDNS and mDNS resolvers at startup. `listen=0/1/2/3` publishes nowhere/on the LAN/via H3/in both scopes; outbound resolution also works with listening disabled. The running process discovers identities and applies enabled/listen/proxy configuration changes automatically. Disabling a Server preserves its listening scope and data; use `genmeta identity ensite --id NAME` or `genmeta identity dissite --id NAME`. See [Server lifecycle](pishoo/docs/server-lifecycle.md). Address changes drive publication. Successful publication renews every 20 seconds; failures retry after 5 seconds. Empty addresses and shutdown stop renewal, and records expire naturally. Pishoo does not actively withdraw records or derive its renewal interval from a server lease header. See the [configuration API](pishoo/docs/config-api.md) for storage, upgrade and initialization rules.

Local proxies support HTTP/3 WebSocket extended CONNECT (`:protocol=websocket`, version 13), translating the upstream HTTP/1.1 upgrade into an H3 200 and streaming opaque frames in both directions. Handshake attempts have bounded timeouts; established sessions have no 30-second lifetime limit. All system HTTP routes are reserved under `/std`, including Lib execution at its declared business path prefixed with `/std`. Missing system paths return 404 and unsupported Lib methods return 405 without reaching proxies. Ordinary `/api/*` and `/file/*` belong to configured proxies independently of loaded Libs. Home Assistant and OpenCode can use their native upstream paths. See [system paths and migration](pishoo/docs/system-paths.md).

Database configuration is available over H3 through `GET/PATCH /std/sys/settings` and `GET/PUT/PATCH/DELETE /std/sys/proxies`. Requests pass daccess authorization and require the same verified identity and owner hash. Writes are transactional; the running process applies proxy and listen changes automatically. See [Configuration API](pishoo/docs/config-api.md) for payloads and native H3 client examples.

First startup with valid identity credentials and an existing `db/config.db` ensures `db/`, `file/`, `libs/std/`, `apps/`, `logs/`, `repo/`, and `templates/` under each identity. New configuration uses `listen=3` (public and LAN scopes) and no proxies. New access databases allow authenticated named identities to `POST /std/contact` and all visitors to `GET /std/acl/capabilities` for capability discovery. All visitors can also read `GET /std/profile` and its avatar/context subpaths, and `HEAD /std/profile/context`, before becoming contacts. The Workspace App keeps the public `/workspace/intro` page; its `/workspace` static pages and assets allow GET/HEAD for all visitors, while management APIs remain protected. Chat capabilities still require approval. Existing configuration, profile data, queues, and user access rules are preserved, including deleted defaults. Initialization is performed by the application, not package installation scripts. See [Initialization and database compatibility](pishoo/docs/config-api.md#启动初始化与数据库兼容).

Existing daccess schema v0 databases are backed up through SQLite before the library upgrades them to v1. The older 0.8.2 `location_rule_sets/location_rules` format is unsupported and stops startup with the original database preserved; it is never silently replaced with new defaults. Missing, corrupt, incomplete, and unsupported databases are distinguished. Pishoo does not create identity credentials, `server.conf`, or an instance configuration file. Linux packages use `User=root` without setting `DHTTP_HOME`. Existing manual overrides remain authoritative; custom deployments can set them in `/etc/systemd/system/pishoo.service.d/`.

Static files under `<identity-home>/<identity>/file/` are served at `/std/file/{path}`. The `/std/file` path itself is unavailable. Ordinary `/file/*` paths use configured proxies; there is no static-file proxy override.

The same-identity DHTTP forwarding route streams request bytes through the native request writer. Request trailers are currently unsupported: declared trailers are rejected before forwarding, and trailers discovered during upload cancel that upload. Response bodies and trailers retain their native streaming behavior.

### Run

Start the development build as the account that owns the identities in `~/.dhttp`. Identities and configuration load at startup; the running process automatically discovers new identities and applies enabled, listen, and proxy changes. Disabling a Server preserves its listening scope and data. Loaded identities require restart or disable/enable to replace certificate/key credentials. Libs load at startup, while online installation/update/removal changes the running Lib collection immediately. SIGHUP requests the existing serial identity/configuration reconciliation. Every 72 hours after startup, Pishoo fetches and validates fresh OCSP proofs for loaded identities, atomically updates their caches, and refreshes TLS registration, DNS publishers, and application outbound credentials. A failed refresh logs the identity and reason and retains its previous in-memory credentials; the next attempt is at the next scheduled refresh. Refreshes do not renew expired identity certificates.

Startup skips identities with invalid credentials, including expired certificates, and logs the identity name and reason. Other identities continue loading. A skipped identity is retried by the periodic identity scan. Invalid configuration, database, or Lib errors still stop startup. Missing or invalid local OCSP caches are prepared during startup; the running process refreshes OCSP every 72 hours, while certificate/key changes require restart.

The `pishoo` executable appends runtime diagnostics to `<identity-home>/logs/error.log` (Linux root default `/etc/dhttp/logs/error.log`; other runs `~/.dhttp/logs/error.log`) and also writes them to stderr. This process-wide log includes shared network diagnostics and identity failures. The default level is `warn`; set `RUST_LOG=debug` for more detail in the same file. New log directories and files use permissions 0700 and 0600 on Unix. Failure to open the log stops startup with a stderr diagnostic. Each listening identity appends request records to its own `logs/access.log`, including the timestamp, server and verified peer names (or anonymous), HTTP version, method, path, and response status. These records are written when response headers are ready, independently of `RUST_LOG`; request bodies and query strings are not logged. HTTP 5xx responses and access-log write failures also produce diagnostics in the global `error.log`. Failure to open an identity’s access log fails its listener setup. Per-identity `logs/cert.log` remains the certificate-operation history written by gmutils.

For an interactive Echo over QUIC, use an already running Pishoo endpoint and a configured local DHTTP identity:

```sh
./pishoo/examples/echo-interactive.py \
  --transport quic --identity client.dhttp.net \
  --url https://server.dhttp.net/std/echo
```

This example assumes an installed Lib declaring `POST /echo`, exposed as `/std/echo`. The endpoint must be reachable, and daccess must allow that source identity to call it. The example builds the native client and opens one full-duplex POST. Type lines and see each `echo>` reply without ending the upload; Ctrl-D finishes the request and Ctrl-C exits the client.

### Testing

Run the normal Rust checks from a release checkout with its registry lockfile:

```sh
cargo test --locked --workspace -- --test-threads=1
cargo check --locked --workspace --all-targets
```

For isolated real QUIC/H3 transport tests:

```sh
python3 pishoo/tests/run-transport.py --soak-seconds 1800
python3 pishoo/tests/run-transport.py --case process
```

The transport runner creates temporary test identities and local upstreams.
It requires Python 3, OpenSSL 3, cached Cargo dependencies, local TCP/UDP access
and about 1.3 GiB of free fixture space. Set `DHTTP_TEST_OPENSSL` to select OpenSSL 3.
Results and logs are written under `target/transport-*`. Local transport tests
do not establish cross-device, NAT or release-build performance.

Named clients and servers require valid OCSP staples as well as valid certificate
chains, names and handshake signatures. Anonymous callers follow the configured
anonymous authorization rules. Older named clients that omit OCSP must upgrade.

### Build your own Lib

The [package management review](pishoo/docs/package-management.md) is the current entry point for host APIs, database schema, install/update/removal, file layout, and known review issues. See the [source package format](pishoo/docs/package-spec.md) for WEBC manifests and signatures, and the [genmeta command reference](pishoo/docs/genmeta-package.md) for CLI behavior. These describe the current working tree; independent store compatibility remains pending.

The [Pishoo WASM developer guide](pishoo/docs/developing-wasm.md) covers writing a WASI HTTP backend. Package metadata and runtime dependencies belong to the WEBC manifest; Lib OpenAPI describes business APIs and is embedded in the installed component. App packages contain frontend resources and no backend OpenAPI.

For Rust components using Pishoo identity functions, use the [developer SDK](pishoo/docs/sdk.md) and the [Journal example](pishoo/examples/journal/README.md). Run `python3 pishoo/tools/package-sdk.py` to produce a verified SDK `.crate` and a standalone developer archive with Journal. The SDK directly exports the existing WIT bindings and is not yet published to crates.io.

1. Build the WASI HTTP backend and separate browser frontend, with the required package metadata and exact Lib dependencies.
2. Use `genmeta build` to create a signed WEBC release locally. This does not certify compatibility with a running host or the unchanged store.
3. Publish the signed source package to a compatible store, then install its exact release with `genmeta install`. Workspace uses the same store-only installation path; local WEBC upload and `--file` installation are not supported. Newly built embedded-OpenAPI Libs remain blocked by the unchanged store validator, as documented in the package management review.

Online installation/update/removal publishes runtime changes immediately. `pishoo reload` is for identity/configuration reconciliation and is not an extra package activation step.

WEBC remains the signed store publication/download input. The host verifies downloaded source packages during installation/update, then stores runtime files at `libs/<name>.wasm` (or `libs/std/<name>.wasm` for standard Libs) and `apps/<name>/public/`. Each App/Lib table contains only seven columns: name, author, version, title, description, license and repository. It stores no local source digest, author-key pin, original package proof, manifest or signature sidecar, and does not perform later local anti-tamper checks. New Lib business data uses `db/<name>/`; an existing legacy `db/<author>/<name>/` can be read without creating new author directories or automatically migrating old data. Names are unique across authors within each type; an App and Lib may share a name. OpenAPI is embedded in the installed WASM. Management exposes one store-only JSON PUT for installation/update, plus DELETE and database list/detail reads; POST returns 405 and raw package PUT returns 415. Auxiliary check/openapi/loaded routes are removed. OCSP is cached at `ssl/stapling.ocsp`. Package removal preserves business data and access rules.

Lib compilation uses Wasmtime's persistent machine-code cache. Installation validation, runtime loading, and later service starts reuse compiled code when the WASM content, compiler version, and engine settings match. Each load reads and prepares installed runtime resources, including necessary compilation/linking; source signature and package-policy checks occur at installation/update. Wasmtime uses its platform cache directory and optional standard `wasmtime/config.toml` configuration; no cache setup is required. Cache initialization failures log a warning and fall back to ordinary compilation. A newly built WASM still needs its first compilation. Development builds optimize Cranelift and its register allocator to reduce this first-compilation cost while retaining Pishoo debug information and the normal WASM execution optimization level. Building these dependencies initially takes longer; release builds already optimize them.

### Access

You can access Pishoo with [`genmeta-curl`](https://docs.dhttp.net/docs/core-components/utils/cli-curl), [DHttp SDK](https://docs.dhttp.net/docs/core-components/sdk), or [AnySee browser](https://docs.dhttp.net/zh/docs/core-components/anysee).

```bash
genmeta curl https://your.name~/welcome
```

For directive reference and additional reverse-proxy examples, see the official [Pishoo documentation](https://docs.dhttp.net/en/docs/core-components/pishoo).

### Management commands

Use gmutils for identity settings and host APIs. Pishoo itself only runs the host
and accepts `reload`, `--help`, and `--version`.

```sh
genmeta identity ensite --id alice.smith
genmeta identity listen --id alice.smith internal
pishoo reload
genmeta server proxy --id alice.smith /test 127.0.0.1:8080
genmeta install alice/note --version 1.0.0 --lib --id alice.smith
genmeta list --id alice.smith
```

Identity settings update the local database. Proxy and package commands use the
running host's existing authenticated APIs. Manage process start/stop/restart/status
with systemctl or brew services. For a foreground process, use `pishoo reload --pid PID`.
See the [management guide](pishoo/docs/management.md) for the complete mapping.

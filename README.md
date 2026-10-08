<p align="center">
  <img src="https://media.dhttp.net/img/pishoo/pishoo-readme-title.jpg" alt="PISHOO — It's the gateway that keeps your data private and secure." width="900">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-0.8.2-1f6feb?style=flat-square" alt="Version 0.8.2">
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

The current development branch is implementing the [v1 contract](design/README.md).
Its SQLite configuration, completed work, test commands, and remaining
transport work are recorded in [IMPLEMENTATION.md](IMPLEMENTATION.md).
The packaged-release installation instructions below describe the earlier release.

### Install Pishoo

Pishoo supports mainstream Linux distributions and macOS on both Arm and x86 architectures. For a quick deployment, we recommend installing the `gmutils` operations toolkit alongside Pishoo.

#### Linux (Debian 11+)

```sh
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

### Current development configuration

Pishoo uses the running user's `~/.dhttp` by default; set `DHTTP_HOME` to use another identity home. Each Server reads its own `<DHTTP_HOME>/<identity>/db/config.db`; there is no instance configuration file or database. Schema v1 has one `settings(listen)` row and a `proxy_locations(location, proxy_pass)` table. Proxy targets are local HTTP/TCP services, for example `127.0.0.1:8080` or `http://127.0.0.1:8080/api/`. Pishoo installs System DNS, anonymous H3 DDNS and mDNS resolvers at startup. `listen=0/1/2/3` publishes nowhere/on the LAN/via H3/in both scopes; outbound resolution also works with listening disabled. Address changes and leases drive publication, and identity removal or shutdown withdraws records using the loaded credentials. Until the online DDNS upgrade, a successful response without `DHTTP-DNS-Lease-Millis` temporarily uses a 300-second renewal window (zero for withdrawal). This window is not a confirmed server lease; Pishoo renews at most every 10 seconds to cover the deployed 30-second storage lifetime. See [IMPLEMENTATION.md](IMPLEMENTATION.md) for the SQL schema, verification results and current limits.

Database configuration is available over H3 through `GET/PATCH /pishoo/settings` and `GET/PUT/PATCH/DELETE /pishoo/proxies`. Requests pass daccess authorization and require the same verified identity and owner hash. Writes are transactional; proxy and listen changes require restart. See [Configuration API](pishoo/docs/config-api.md) for payloads and native H3 client examples.

First startup with valid identity credentials creates `db/`, `file/`, `lib/`, `logs/`, `repo/`, `templates/`, and `assets/profile/` under each identity. New configuration uses `listen=3` (public and LAN scopes) and no proxies. New access databases allow authenticated named identities to `POST /contact`; Chat capabilities still require approval. Existing configuration, profile data, queues, and user access rules are preserved, including deleted defaults. Initialization is performed by the application, not package installation scripts. See [Initialization and database compatibility](pishoo/docs/config-api.md#启动初始化与数据库兼容).

Existing daccess schema v0 databases are backed up through SQLite before the library upgrades them to v1. The older 0.8.2 `location_rule_sets/location_rules` format is unsupported and stops startup with the original database preserved; it is never silently replaced with new defaults. Missing, corrupt, incomplete, and unsupported databases are distinguished. Pishoo does not create identity credentials, `server.conf`, or an instance configuration file. Run services as the identity owner; systemd installations need an appropriate `User=` and `Environment=DHTTP_HOME=...` override.

Static files under `<DHTTP_HOME>/<identity>/file/` are served at `/file/{path}`. The `/file` path itself is unavailable; configured proxy locations handle other matching paths.

The same-identity DHTTP forwarding route streams request bytes through the native request writer. Request trailers are currently unsupported: declared trailers are rejected before forwarding, and trailers discovered during upload cancel that upload. Response bodies and trailers retain their native streaming behavior.

### Run

Start the development build with `DHTTP_HOME` pointing to the instance directory. Identities, proxy routes, Libs, configuration, and certificate/key credentials are loaded at startup; restart Pishoo to apply changes. SIGHUP reload is not supported. Every 72 hours after startup, Pishoo fetches and validates fresh OCSP proofs for loaded identities, atomically updates their caches, and refreshes TLS registration, DNS publishers, and application outbound credentials. A failed refresh logs the identity and reason and retains its previous in-memory credentials; the next attempt is at the next scheduled refresh. Refreshes do not renew expired identity certificates.

Startup skips identities with invalid credentials, including expired certificates, and logs the identity name and reason. Other identities continue loading. A skipped identity is retried at the next startup. Invalid configuration, database, or Lib errors still stop startup. Missing or invalid local OCSP caches are prepared during startup; the running process refreshes OCSP every 72 hours, while certificate/key changes require restart.

The `pishoo` executable appends runtime diagnostics to `<DHTTP_HOME>/logs/error.log` (default `~/.dhttp/logs/error.log`) and also writes them to stderr. This process-wide log includes shared network diagnostics and identity failures. The default level is `warn`; set `RUST_LOG=debug` for more detail in the same file. New log directories and files use permissions 0700 and 0600 on Unix. Failure to open the log stops startup with a stderr diagnostic. Per-identity `logs/cert.log` remains the certificate-operation history written by gmutils.

For an interactive Echo over QUIC, use an already running Pishoo endpoint and a configured local DHTTP identity:

```sh
DHTTP_HOME=/path/to/home ./pishoo/examples/echo-interactive.py \
  --transport quic --identity client.dhttp.net \
  --url https://server.dhttp.net/api/echo/echo
```

The endpoint must be reachable, and daccess must allow that source identity to call the Echo API. The example builds the native client and opens one full-duplex POST. Type lines and see each `echo>` reply without ending the upload; Ctrl-D finishes the request and Ctrl-C exits the client.

QUIC is the only DHTTP transport. The former TCP mock smoke fixtures remain under `pishoo/tests/` for adaptation; their old script is no longer a working test entry point. Real QUIC/H3 acceptance has passed through online DDNS and a local HTTP upstream, including public relay bootstrap and direct punching between two processes behind the same RestrictedPort NAT. Different NATs and devices remain to be tested; see [IMPLEMENTATION.md](IMPLEMENTATION.md).

Online acceptance lives in the existing `pishoo-client` example, outside unit tests. Set `DHTTP_HOME` and `PISHOO_CLIENT_IDENTITY` to a test identity. Its commands are `query [NAME]`, `publish [ADDRESS ...]`, `probe`, `serve`, and `nat-get URL`. Publication always uses the loaded identity. `serve` starts the real Pishoo application after waiting for the Network-owned mapping; `nat-get` checks HTTP/3 responses before and after punching. Both omit mDNS and withdraw LAN/loopback advertisements for acceptance. Ordinary Network startup now classifies NAT once for each new socket and continuously maintains mappings with STUN binding heartbeats. Filtering NAT publishes relay E-records as `outer-agent`; FullCone mappings can be published directly. These commands reuse that maintenance and do not run a second classification on the same socket.

```sh
cargo run --locked -p pishoo --example pishoo-client -- probe
# In separate processes, with a prepared test configuration and upstream:
cargo run --locked -p pishoo --example pishoo-client -- serve
cargo run --locked -p pishoo --example pishoo-client -- nat-get https://server.dhttp.net/path
```

`serve` follows the test identity's `listen` configuration and publishes/withdraws its online records. Use an isolated profile and restore any pre-existing records after acceptance. Unit tests only exercise local logic; none of these commands run automatically.

Peer OCSP staples are temporarily optional for all server domains. Certificate chain, hostname, validity and handshake signature checks remain enabled; supplied OCSP responses still require certificate binding, signature, freshness and good status. For `ddns.genmeta.net`, an explicitly configured `DQUIC_DDNS_OCSP_FILE` remains an optional fallback and must pass those same OCSP checks. Ordinary startup does not need this file.

The Lib sources are in `pishoo/examples/wasm-demo/`. Prebuilt components are included; to rebuild them, run `./pishoo/tools/build-wasm-demo.sh` with `wasm-tools` and the Rust `wasm32-unknown-unknown` target installed.

### Build your own Lib

1. Write a WASI HTTP component exporting `wasi:http/incoming-handler@0.2.12#handle`. The [Echo source](pishoo/examples/wasm-demo/echo/src/lib.rs) is a working Rust example. Build it for `wasm32-unknown-unknown`, then turn the core WASM into a component with `wasm-tools component new` as shown in [build-wasm-demo.sh](pishoo/tools/build-wasm-demo.sh).
2. Write an OpenAPI 3.1 JSON document declaring the paths and HTTP methods the Lib accepts. See [echo/openapi.json](pishoo/examples/wasm-demo/echo/openapi.json). The handler's actual response statuses should match the document.
3. Attach that JSON to the component, check it with Pishoo, and place the result in the identity profile's `lib/<id>/lib.wasm`:

```sh
./pishoo/tools/package-lib.py component.wasm openapi.json /path/to/identity/lib/echo/lib.wasm
wasm-tools validate /path/to/identity/lib/echo/lib.wasm
cargo run --locked -p pishoo --example check-lib -- /path/to/identity/lib/echo/lib.wasm
```

The file must be one WASM component with exactly one top-level `pishoo:openapi` custom section. Pishoo reads this section without executing the guest. It routes `POST /api/echo/echo` to Lib `echo` only when its manifest declares `POST /echo`. On load, Pishoo registers each declared method and public path in daccess with a default deny rule if that method and path have no rule yet. Grant access through daccess; the OpenAPI declaration itself does not grant access.

### Access

You can access Pishoo with [`genmeta-curl`](https://docs.dhttp.net/docs/core-components/utils/cli-curl), [DHttp SDK](https://docs.dhttp.net/docs/core-components/sdk), or [AnySee browser](https://docs.dhttp.net/zh/docs/core-components/anysee).

```bash
genmeta curl https://your.name~/welcome
```

For directive reference and additional reverse-proxy examples, see the official [Pishoo documentation](https://docs.dhttp.net/en/docs/core-components/pishoo).

### Management commands

Local commands use `DHTTP_HOME` (otherwise `~/.dhttp`) and the initialized identity's files. Select with `--id NAME` or `-i NAME` before or after the resource; without it, `settings.toml` must define `[default].name`. Queries write stdout, identity/save notices write stderr. Resource changes require a service restart.

```sh
pishoo listen -i alice.smith internal
pishoo proxy -i alice.smith /test 127.0.0.1:8080
pishoo proxy -i alice.smith rm /test
pishoo lib check ./note.wasm
pishoo lib -i alice.smith install note ./note.wasm
pishoo lib -i alice.smith
pishoo restart
pishoo lib -i alice.smith --loaded
pishoo lib -i alice.smith rm note
```

`start`, `stop`, `restart`, and `status` manage the entire installed systemd/Homebrew service, with its existing user and home. No automatic sudo or service installation is performed. No arguments runs all identities in the foreground. Lib removal preserves `db/<id>` and ACL rules. See the [management design](pishoo/docs/management-design.md) for commands, `/pishoo` HTTP APIs, atomic file publication, and error behavior. Old `/sys` management paths have no compatibility aliases.

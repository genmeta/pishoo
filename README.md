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
transport/exec work are recorded in [IMPLEMENTATION.md](IMPLEMENTATION.md).
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

Set `DHTTP_HOME` before starting Pishoo. Each Server reads its own `<DHTTP_HOME>/<identity>/db/config.db`; there is no instance configuration file or database. Schema v1 has one `settings(listen, exec)` row and a `proxy_locations(location, proxy_pass)` table. Proxy targets are local HTTP/TCP services, for example `127.0.0.1:8080` or `http://127.0.0.1:8080/api/`. `exec=1` enables `POST /exec` for the same verified identity; it runs one host command with the Pishoo service account's permissions. See [IMPLEMENTATION.md](IMPLEMENTATION.md) for the SQL schema and current behavior.

Static files under `<DHTTP_HOME>/<identity>/file/` are served at `/file/{path}`. The `/file` path itself is unavailable; configured proxy locations handle other matching paths.

### Run

Start the development build with `DHTTP_HOME` pointing to the instance directory. To reload identities, proxy routes, and Libs in the running process, send `SIGHUP` to the Pishoo process (`kill -HUP <pishoo-pid>`). Changes to `listen` or `exec` require a restart.

For an interactive Echo over the TCP stream backend, run:

```sh
./pishoo/examples/echo-interactive.py --transport tcp
```

This builds the native client, creates a temporary sample identity, starts Pishoo, and opens **one full-duplex POST**. Type lines and see each `echo>` reply without ending the upload. Ctrl-D finishes the request; Ctrl-C exits and stops the temporary server.

The same example can use QUIC with an already running Pishoo endpoint and a configured local DHTTP identity:

```sh
DHTTP_HOME=/path/to/home ./pishoo/examples/echo-interactive.py \
  --transport quic --identity client.dhttp.net \
  --url https://server.dhttp.net/api/echo/echo
```

The QUIC endpoint must already be reachable, and daccess must allow that source identity to call the Echo API. To connect to an existing TCP mock server, supply `DHTTP_HOME`, `DHTTP_TCP_MOCK_PORTS`, `--transport tcp`, `--identity`, and `--url`.

The `tcp-mock` build runs the normal `pishoo` binary against any existing `DHTTP_HOME` profile. `DHTTP_TCP_MOCK_PORTS` maps each identity name to a loopback port. The sample setup is separate from the server: it explicitly allows anonymous access to the static/proxy paths used by the demo and reads the final `lib.wasm` OpenAPI sections to allow the demo Lib routes. The smoke script starts a local HTTP server for the proxy upstream. Other anonymous paths remain denied; TCP transport does not grant access.

The TCP adapter carries h3x bidirectional request streams and unidirectional control/QPACK streams over loopback TCP. It exercises dhttp's H3 request/response and Body adapters, Pishoo routes, the local HTTP proxy with a duplex upload/response, and WASM execution. It does not exercise QUIC, TLS peer authentication, or path discovery. The automated TCP test is in [tests/h3x-tcp-smoke.sh](pishoo/tests/h3x-tcp-smoke.sh).

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

<p align="center">
  <img src="https://media.dhttp.net/img/pishoo/pishoo-readme-title.jpg" alt="PISHOO — It's the gateway that keeps your data private and secure." width="900">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-0.8.2--beta.1-1f6feb?style=flat-square" alt="Version 0.8.2-beta.1">
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

### Configuration files

- Linux global configuration: `/etc/dhttp/pishoo.conf`
- macOS global configuration: `$(brew --prefix)/etc/dhttp/pishoo.conf`
- Per-identity service configuration: `<DHTTP home>/<identity>/server.conf`, for example `~/.dhttp/your.name/server.conf`

Pishoo loads the global configuration and then discovers `server.conf` files for users in the platform worker group: `dhttp` on Linux and `_www` on macOS. The identity directory also contains the generated certificate and private key under `ssl/`; do not rename or manually change their permissions.

Add the account that owns the identity to the worker group:

```sh
# Linux
sudo usermod -aG dhttp "$USER"

# macOS
sudo dseditgroup -o edit -a "$USER" -t user _www
```

### Configuration example

The global file can remain minimal. The packaged default configuration only needs a PID file when the platform default worker group should be discovered automatically:

```nginx
# /etc/dhttp/pishoo.conf on Linux
pishoo {
    pid /var/run/pishoo.pid;
}
```

Put the service configuration in your identity directory. Pishoo derives the service identity and TLS material from that directory, while the following example serves a static site and proxies `/app` to a local application:

```nginx
# ~/.dhttp/your.name/server.conf
server {
    location / {
        root  templates;
        index index.html;
    }

    location /app {
        proxy_pass http://127.0.0.1:8081;
    }
}
```

### Run

Validate and start (or reload) Pishoo after changing its configuration:

```sh
# Linux
sudo pishoo -t
sudo systemctl start pishoo
sudo systemctl reload pishoo

# macOS
sudo pishoo -t
sudo brew services start pishoo
sudo brew services reload pishoo
```

### Access

You can access Pishoo with [`genmeta-curl`](https://docs.dhttp.net/docs/core-components/utils/cli-curl), [DHttp SDK](https://docs.dhttp.net/docs/core-components/sdk), or [AnySee browser](https://docs.dhttp.net/zh/docs/core-components/anysee).

```bash
genmeta curl https://your.name~/welcome
```

For directive reference and additional reverse-proxy examples, see the official [Pishoo documentation](https://docs.dhttp.net/en/docs/core-components/pishoo).

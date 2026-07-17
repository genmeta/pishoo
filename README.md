# gateway

## Pishoo Multi-Process Supervisor

Pishoo is a privilege-separated, multi-process reverse proxy supervisor. The root process manages worker lifecycle, privileged resources (PID file, QUIC listeners), and server ownership registry. Individual worker processes (each running under a dedicated user) handle business logic, per-server routing, and connection handling.

### Architecture

- **Root supervisor**: Starts/stops workers, maintains `uid -> worker` and `server_name -> owner worker` registries, forwards connections to owning workers, forwards system signals.
- **Worker processes**: Each runs under a dedicated unprivileged user. Owns user identity services, router construction, TLS cert/key handling, and business proxy behavior through the user's DHTTP home.

### Configuration

Default `pishoo` startup loads the global DHTTP home:

```text
<global DHTTP home>/pishoo.conf
<global DHTTP home>/<identity>/server.conf
```

The main process owns global identity services and pishoo config services. Worker
processes own user identity services for their Unix users. Both global and user
identity services load identity profiles through the DHTTP home API.

Use `pishoo -c <file>` only for a standalone config file. Explicit config mode
does not infer a DHTTP home, does not load identity profile `server.conf` files,
and does not enumerate the platform default worker group (`dhttp` on non-macOS,
`_www` on macOS) when `workers` and `groups` are absent.

### 启动反向代理

```sh
cargo run -p pishoo
```

This starts the root supervisor, which then launches each configured worker process.

### 启动正向代理

```sh
cargo run -p gateway --example forward config/forward.conf
```

### 测试请求

```sh
curl -x http://127.0.0.1:5379 http://test2.dhttp.net/static/TODO.md
```

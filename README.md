# gateway

## Pishoo Multi-Process Supervisor

Pishoo is a privilege-separated, multi-process reverse proxy supervisor. The root process manages worker lifecycle, privileged resources (PID file, QUIC listeners), and server ownership registry. Individual worker processes (each running under a dedicated user) handle business logic, per-server routing, and connection handling.

### Architecture

- **Root supervisor**: Starts/stops workers, maintains `uid -> worker` and `server_name -> owner worker` registries, forwards connections to owning workers, forwards system signals.
- **Worker processes**: Each runs under a dedicated unprivileged user. Owns per-server identity configuration, router construction, TLS cert/key handling, and business proxy behavior. Identities are loaded from `~/.genmeta/<identity>/` directories.

### Configuration

Root config (`pishoo.conf`) is a supervisor-level configuration that specifies:

```
pishoo {
    pid /var/run/pishoo.pid;      # (optional) PID file path; defaults to /var/run/pishoo.pid
    workers alice bob charlie;    # list of worker usernames to supervise
}
```

Per-server configuration lives in each worker's identity directory: `~/.genmeta/<identity>/pishoo.conf`, not in the root config.

### 启动反向代理

```sh
cargo run -p pishoo -- -c config/pishoo.conf
```

This starts the root supervisor, which then launches each configured worker process.

### 启动正向代理

```sh
cargo run -p gateway --example forward config/forward.conf
```

### 测试请求

```sh
curl -x http://127.0.0.1:5379 http://test2.genmeta.net/static/TODO.md
```

### HTTPS upstream 端到端验证

```sh
scripts/e2e_proxy_https_chain.sh
```

# Changelog

## Unreleased

## [0.8.0-beta.6] - 2026-07-17

### Changed

- Non-macOS default worker discovery now uses the `dhttp` group, and Linux
  DEB/RPM installation hooks ensure that group exists instead of creating the
  legacy `pishoo` group. macOS continues to use `_www`.
- `pishoo-common` now follows the current pishoo source version. Linux packages
  render it as `0.8.0~beta.6-1`, while pishoo accepts common packages from the
  last published `0.5.1-1` through its current package version.

### Fixed

- pishoo identity services without an explicit `access_rules` directive once
  again load the identity profile's `db/access.db`; only a genuinely absent
  implicit database falls back to an empty policy, while explicit or damaged
  databases fail service preparation.

### Components

- `pishoo` v0.8.0-beta.6
- `gateway` v0.8.0-beta.5 (unchanged)
- `pishoo-common` v0.8.0-beta.6 (`0.8.0~beta.6-1` for DEB/RPM)

## [0.8.0-beta.5] - 2026-07-16

### Added

- pishoo builds inherited per-account DHTTP home trees and compiles them into a
  static service pipeline for reload execution.

### Changed

- Gateway configuration parsing now uses role-aware typed domains, sealed
  inherited trees, and typed compound STUN server values.
- Reload snapshots and service plans now enforce exhaustive, immutable parser
  and transport contracts before worker activation.

### Fixed

- Cascaded and inherited configuration queries preserve typed identity and
  registry contracts, including native OS paths for SQLite databases.
- Slash-directory proxy requests redirect before fallback routing is evaluated.
- IP-family parse errors compile cleanly with the release packaging nightly toolchain.

### Dependencies

- Release manifests now target `dhttp` v0.6.0-beta.4, including
  `dhttp-access` v0.4.0-beta.2 and `dhttp-home` v0.5.0-beta.1 through
  the facade; discovery and transport dependencies target `dyns`
  v0.7.0-beta.2, `h3x` v0.6.0-beta.4, `dquic` v0.7.0-beta.4, and
  `dshell` v0.6.0-beta.3.

### Components

- `gateway` v0.8.0-beta.5
- `pishoo` v0.8.0-beta.5
- `pishoo-common` v0.5.1-1 (unchanged)

## [0.8.0-beta.4] - 2026-07-09

### Added

- pishoo now keeps identity access-rule databases as live policy sources instead
  of loading only a static in-memory snapshot.
- Gateway and pishoo access checks now evaluate rules through the shared
  `dhttp` access policy evaluator trait.

### Changed

- pishoo enables the `dhttp` access ORM facade feature for access-rule database
  evaluation.

### Fixed

- Release workflows upload package assets from publish reports instead of broad
  local artifact globs.

### Dependencies

- Release manifests now target `dhttp` v0.5.0-beta.3, `dhttp-access`
  v0.4.0-beta.1 through the `dhttp` facade, `dhttp-home`
  v0.4.0-beta.1, `dhttp-identity` v0.3.0-beta.1, `dyns`
  v0.6.0-beta.3, `h3x` v0.6.0-beta.3, and `dshell`
  v0.6.0-beta.2.

### Components

- `gateway` v0.8.0-beta.4
- `pishoo` v0.8.0-beta.4
- `pishoo-common` v0.5.1-1

## [0.8.0-beta.3] - 2026-07-06

### Fixed

- Release packaging now keeps the `pishoo-common` sidecar package in preview
  DEB/RPM repository metadata when publishing preview pishoo packages.

### Components

- `gateway` v0.8.0-beta.3
- `pishoo` v0.8.0-beta.3
- `pishoo-common` v0.5.1-1

## [0.8.0-beta.2] - 2026-07-06

### Fixed

- pishoo now loads identity access rules from the identity profile access-rule database.

### Dependencies

- Release manifests now target `h3x` v0.6.0-beta.2, `dhttp`
  v0.5.0-beta.2, and `dshell` v0.6.0-beta.2.

### Components

- `gateway` v0.8.0-beta.2
- `pishoo` v0.8.0-beta.2
- `pishoo-common` v0.5.1-1

## [0.8.0-beta.1] - 2026-07-02

### Fixed

- macOS worker discovery now uses the existing `_www` group and checks group
  membership through macOS membership APIs instead of bounded supplementary
  group lists.
- Worker process credential setup now initializes group state with the platform
  `initgroups` path instead of carrying a precomputed supplementary group list
  from the parent process.

### Changed

- Homebrew installation no longer tries to create the `pishoo` group. Operators
  who want default worker discovery on macOS can add users to `_www` manually.
- Prepared pishoo for the DHTTP beta dependency line.

### Dependencies

- Release manifests now target `h3x` v0.6.0-beta.1, `dhttp`
  v0.5.0-beta.1, `dshell` v0.6.0-beta.1, and `dyns` v0.6.0-beta.1.

### Components

- `gateway` v0.8.0-beta.1
- `pishoo` v0.8.0-beta.1
- `pishoo-common` v0.5.1-1

## [0.7.0] - 2026-06-24

### Added

- Release packaging now reads the pishoo package contract from
  `xtask/release.toml`, including Homebrew templates and target-local build
  environment overrides.
- Gateway config parsing now resolves config-relative pishoo paths.
- pishoo now loads workers from the `pishoo` system group by default when
  `workers/groups` are omitted, including primary-group users.

### Changed

- pishoo registered endpoints now apply the DHTTP default endpoint behavior when
  constructing root-owned listeners.
- Packaging uses the normalized manifest-first S3/R2 release contract.
- pishoo DEB/RPM/Homebrew packaging now creates or explains the `pishoo` group
  best-effort, and runtime startup warns instead of failing when the default
  group is absent.

### Dependencies

- Release manifests now target `h3x` v0.5.0, `dhttp` v0.4.0,
  `dshell` v0.5.0, and `dyns` v0.5.0.

### Components

- `gateway` v0.7.0
- `pishoo` v0.7.0
- `pishoo-common` v0.5.1-1

## [0.6.0] - 2026-06-15

### Added

- **DHTTP endpoint-backed listener and publisher flow**: gateway listeners are
  routed through the endpoint facade and publish endpoint records through the
  DHTTP DNS publisher path.
- **WebTransport DShell service path**: pishoo SSH service now accepts DShell
  sessions through WebTransport Extended CONNECT. The root process transfers
  accepted WebTransport sessions over IPC, and workers build DShell
  conversations on the worker side.
- **Span-aware configuration parser**: the legacy directive parser has been
  replaced by a typed parser with source spans, include tracing, structured
  diagnostics, and directive-specific validation.
- **Per-server worker runtime model**: pishoo workers prepare server services
  into runtime snapshots, reload unchanged listeners in place, and retire stale
  services through guarded runtime transitions.
- **Listener resource registry**: root-owned listeners now have explicit
  acquire/release/rebuild operations with stale-drop protection and conflict
  handling across workers.
- **Manifest-first release packaging**: `cargo xtask package` and
  `cargo xtask publish s3` now stage and publish pishoo DEB, RPM, and Homebrew
  artifacts from package manifests.
- **pishoo-common release contract**: `xtask/release.toml` records the common
  package version and the minimum common package version required by current
  pishoo binaries.

### Changed

- Gateway source imports now consume the DHTTP stack through the `dhttp` facade
  (`dhttp::h3x`, `dhttp::ddns`, `dhttp::access`, and identity/home re-exports).
- Access-rule loading uses the DHTTP access-control re-export and constructs the
  in-memory matcher from the configured SQLite store without a direct
  `dhttp-access` package dependency.
- SSH service wiring now uses `dshell` directly. The previous stream-oriented
  SSH path has been replaced by the WebTransport conversation API.
- `proxy_pass` URI rewriting is aligned with nginx-style prefix semantics.
- `listen internal` is localhost-only by construction. External listen scopes
  remain rejected with typed configuration errors.
- Gateway no longer owns DNS certificate-chain-key configuration or DNS bootstrap constants.
  DNS publication is bridged through DHTTP/DDNS endpoint publication APIs.
- `pishoo-common` remains at `0.5.0-1`; the `pishoo` binary package advances to
  `0.6.0-1` and declares its compatible common-package range.

### Fixed

- Reload and cancellation paths now keep listener creation, listener release,
  worker cleanup, and endpoint teardown inside owned transition tasks.
- Worker process lifecycle is UID-keyed and typed: IPC disconnects, startup
  failures, and child exits flow through explicit worker process errors.
- FD transfer now follows the receiver-chosen, remoc-cancellation-visible
  contract used by the root/worker mux.
- SIGHUP/SIGTERM/SIGINT/SIGQUIT/SIGUSR1 handling was aligned with the pishoo
  signal contract.
- AArch64 GNU package builds filter the unsupported Zig/Rust linker mitigation
  flag, and RPM release CI omits the unsupported Fedora armv7/armhfp target.

### Dependencies

- Release manifests now target the DHTTP/DShell release-wave crates: `h3x`
  v0.4.0, `dhttp` v0.2.0, and `dshell` v0.4.0.

### Components

- `gateway` v0.6.0
- `pishoo` v0.6.0
- `pishoo-common` v0.5.0-1

## [0.5.0] - 2026-04-20

### Added
- **dhttp-home identity model**: pishoo now adopts `dhttp-home` as the
  foundational identity abstraction. Each OS user owns a *dhttp home*
  that contains any number of *identity homes*, each holding one
  identity's TLS certificate/key, server configuration, and related
  assets. The gateway resolves services per identity home rather than
  consuming a single monolithic config.
- **Privilege-separated multi-process supervisor**: motivated directly by
  dhttp-home, pishoo splits into a privileged root process and per-user
  worker processes. The root owns listeners, the PID file, and a
  `server_name -> owner worker` registry; each worker runs as its owning
  OS user and serves the identity homes that live in that user's dhttp
  home. This is what makes it correct to host many users' identities on
  one gateway without running business logic as root.
- **Standalone STUN server mode**: new `stun_server { bind / outer_addr /
  change_addr / change_port }` directives for running RFC 5780 STUN
  endpoints inside a `server` block.
- **Access control plane**: new `access_rules sqlite://...` directive
  backed by a SQLite ACL database, plus an HTTP configuration API for
  rule management.
- **Per-identity access logs**: non-blocking writer producing structured
  access logs scoped to each identity.
- **Response compression**: `gzip`, `gzip_comp_level`, `gzip_min_length`,
  `gzip_vary`, and `gzip_types` directives.
- **Header directives**: `proxy_set_header` and `add_header` with variable
  interpolation (`$host`, `$scheme`, `$http_*`, `$arg_*`, `$remote_addr`).
- **Upstream TLS**: configurable TLS to proxied upstreams.
- **SIGHUP-driven selective reload**: listeners are reused where possible
  when only per-worker configuration changes.
- **xtask distribution tooling**: `cargo xtask` replaces the shell /
  Makefile pipeline with parallel builds, shared cargo cache,
  `dpkg-buildpackage` + debhelper based `.deb` generation, Homebrew
  formula generation, and cross-compilation for `amd64`, `arm64`,
  `armhf`, `i686`, and macOS Apple Silicon / Intel.
- README rewritten to document the new supervisor architecture and boot
  flow.

### Changed
- Switched to the h3x / dquic 0.2 line; forward and reverse data paths
  migrate to h3x `TowerService`, `h3x::quic::Listen`, and
  `Arc<Connection>` propagation for reduced per-request allocation.
- TLS certificate/key resolution is delegated to dhttp-home `Identity`
  instead of ad-hoc file loading.
- Forward proxy client certificate fields now refer to per-identity
  keychain paths.
- DNS publishing now publishes empty records when no endpoints are
  available, clearing stale entries instead of leaving them.
- STUN configuration: the `STUN_SERVER` environment variable is removed;
  the built-in default server is now `nat.genmeta.net:20004` (was
  `stun.genmeta.net:20002`) and can be overridden via config.
- Missing files on reverse-proxied paths return HTTP 404 instead of 500.
- IPC between supervisor and workers uses a multiplexed channel
  transport.
- Upgrade `nix` 0.30 → 0.31.

### Removed
- **SSH3 password / basic authentication**: `ssh_login basic` is no
  longer accepted; only `ssh_login ssl` (client-certificate
  authentication) is supported.
- `STUN_SERVER` environment variable (use config instead).
- Legacy `Makefile` / `homebrew.sh` / `pishoo/pkg` packaging artifacts
  (superseded by `xtask`).

### Fixed
- `sshd`: prevent lingering `pishoo-ssh-session` processes after client
  disconnect; register the conversation before returning 200 OK.
- Pishoo: use SOCK_CLOEXEC fallback on macOS; close leaked seqpacket
  sockets; tolerate worker spawn failures without crashing the root.
- DNS: spawn interface teardown in the background to prevent the
  reconcile loop from blocking on slow closes.

### Dependencies
- Pin all git dependencies to specific revisions (`rev = "..."`) of the
  respective repositories' default branches to avoid accidental drift.
- h3x is the only git dependency over `https://`; all others use
  `ssh://`.


## [0.4.2]

- 响应运行时动态新增&移除网卡
- sshd3正确添加所有用户组
- 修复mdns socket泄漏问题
- 立刻的网络变化响应，而不只是定时器

## [0.4.1]

- 使用genmeta-buildx构建系统自动打包
- 修复mdns发布的地址带有端口0
- 修复不能绑定特定端口
- 修复pishoo-common更新会覆盖原有配置文件
- 修复pid文件会被覆盖的问题
- 更新了systemd服务文件，支持reload，修复注释错误
- 分离了sshd3的实现到单独仓库单独crate
- sshd正确使用pam，设置进程组ID，创建和结束session
- 其他诸多琐碎问题...

## [0.4.0]

- 结合acces对客户端进行认证
  - 支持ssl免密登录，将用户名加入path，同时保持对旧客户端的兼容性
- 整理日志和错误汇报
- 修复信号处理
- 结合gm-quic 0.3的QuicListener进行并行DNS汇报

## [0.3.1]

- 更新gm-quic-traversal依赖，适配windows

## [0.3.0]

- 适配使用gm-quic 0.3
- 改进超时机制
- 改进resume(restart)处理
- 不再使用udp resolver

## [0.2.8]

### 修复

- MDNS 导致崩溃

### 更新

- 提升打洞效率

## [0.2.7]

### 更新

- 不汇报`test`和`user`域dns
- 更新依赖（traversal）
- 在ssh3 auth 提示中回显uri（不包括.genemta.net）

## [0.2.6]
### 修复
-   转发请求时, 删除了过多的 Header, 导致部分请求失败的问题

## [0.2.5]

### 更新
-   location 支持 `proxy_set_header` 配置
    -   当前支持变量:
        -   `$host`
        -   `$scheme` 变量
        -   以 `$http_` 开头的变量, 例如 `$http_user_agent`, 将匹配原始请求 `User-Agent` 头部
        -   以 `$arg_` 开头的变量, 例如 `$arg_user_id`, 将匹配query字符串中的请求参数 `user_id`/`user-id`/`user.id` 值
        -   `$remote_addr` 变量, 匹配客户端 IP 地址
    -   目前仅支持单独设置变量, 或者直接设置为常量值, 不支持变量拼接
-   proxy_pass 地址支持末尾斜杠
-   代理响应默认添加 CORS 相关头部
-   代理请求时, 默认将 `Host` 头部设置为目标地址, `Connection` 头部设置为 `close`, 去除其他 Header
-   支持 https dns

## [0.2.4]

### 修复

-   多 server 块支持绑定同一端口
-   支持 ~ 后缀
    -   使用 `http://test~` 可以访问 `https://test.genmeta.net`
-   mdns 支持

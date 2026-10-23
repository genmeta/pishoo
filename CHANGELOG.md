# Changelog

## [0.9.0-preview.5] - 2026-10-23

### Changed

- Upgrade an application and its exact direct and transitive dependencies in one
  installation transaction. Reuse matching loaded dependencies, prepare runtime
  resources before committing, and recover the entire batch after interruption.
  Conflicting requirements from other installed applications reject the update
  and identify the dependent packages and required versions.
- Allow the same author's installed package to add or remove APIs, change HTTP
  methods, and replace or clear capability declarations while retaining existing
  ACL rules. These rules continue to govern requests after the upgrade.
- Use `author`, `author_owner_hash` and `dependencies[].author` in store metadata,
  and download signed packages through `package.waz`. Releases now use standard
  ZIP, `application/zip`, a signed JSON manifest and optional README/CHANGELOG
  documents. Reject legacy WEBC release inputs; existing installed resources and
  private data remain unchanged.
- Return pending access reviews as HTTP 202 with `status: pending_review`, a review
  ID and a status URL. Keep queued business responses distinct; waiting reviews
  report denial, timeout and unavailable channels as 403, 504 and 503 respectively.
- Support once and remembered access decisions, localized permission-group titles,
  and optional permission-directory details.
- Initialize only native contact and permission-discovery ACL defaults. Business
  packages require owner-configured access; existing rules are preserved and no
  defaults file is read or reapplied on restart.

### Fixed

- Preserve App root ACLs when frontend asset filenames change.
- Persist authenticated contact delivery review state in contact schema v5 and
  poll the original review while pending. Denial or expiry ends the delivery task
  without replaying the business request or changing the contact relationship.
- Repair account ownership across personal `.dhttp` directories when the host
  runs as root, preserving file modes and filesystem boundaries.

### Dependencies and compatibility

- Use the published daccess 0.2.0 crate from crates.io.
- Deploy the matching ZIP `author` / `.waz` registry protocol before using store
  installation or updates. Old `publisher` / `.pkg` HTTP responses are unsupported.
- Publish compatible application releases and their exact dependencies separately;
  host distributions do not contain or automatically install business packages.
  Clients must distinguish pending reviews from queued business success.
- Existing ACLs and private business data are retained. Changing a package's author
  is not an ordinary upgrade; conflicting authors cannot replace the same short name.

Pishoo and pishoo-common advance together to 0.9.0-preview.5. The SDK remains
0.1.0-preview.2.

## [0.9.0-preview.4] - 2026-10-21

### Changed

- Manage one signed Pkg containing a Lib, an App, or both through the unified
  `/std/sys/pkg` API; commit its runtime files, registration, dependencies and
  permission declarations together with recovery on failure.
- Separate listener settings from local HTTP mounts using `/std/sys/listen`
  and `/std/sys/mount`, and use `listen off` to stop hosting.
- Mount all frontends under `/app/<name>/` with one root GET authorization;
  expose verified viewer names through existing App HEAD responses.
- Dispatch Lib requests by loaded package name, keeping native `/std` routes
  first and using OpenAPI for validation, documentation and permission groups.
- Store native databases under `.volume/sys/<service>/database` and give each
  Lib a private volume mapped to `/db`.
- Move Workspace, Profile, Message, Calendar and Journal into independent product
  repositories; host distributions and SDK bundles contain no product sources.
- Align Contact orchestration with query-based ACL resources and the current
  authenticated application protocol, including operation snapshots and subject
  binding. Use Profile directly for contact avatars.
- Pin daccess to commit `1bd97a7d2e8bd572cde700b1ec52c8ca8fca2ee4`.

### Compatibility

- Use the unified Pkg registry protocol and matching gmutils commands. Profile
  0.2.0 exposes independent nickname/avatar fields; Message 0.2.0 uses PUT.
  Business packages remain separately published and are not installed by the host.
- Existing ACL entries are not automatically rewritten when methods or paths
  change. Installation does not grant business API access.

Pishoo and pishoo-common advance together to 0.9.0-preview.4. The SDK remains
0.1.0-preview.2.

## [0.9.0-preview.3] - 2026-10-15

### Changed

- Require an existing config.db before loading an identity, so browser-only
  credentials do not silently enable hosting.
- Move management and package-building commands to gmutils. Keep the host APIs,
  foreground runtime, help/version and explicit SIGHUP configuration reload.
- Stop shipping pishoo-pack in host DEB/RPM/Homebrew packages.

### Fixed

- Append request records to per-identity access.log independently of RUST_LOG;
  report HTTP 5xx and log-write failures through the global error.log.

- Remove a stopped identity from every local mDNS responder before waiting for
  guest tasks. Cover disabling, removal, internal-listener shutdown and process
  exit while retaining other identities and shared query resources.
- Stop public address renewal without sending a withdrawal request.

Pishoo and pishoo-common advance to 0.9.0-preview.3. The unchanged SDK remains
0.1.0-preview.2; business App/Lib versions are published independently.


## [0.9.0-preview.2] - 2026-10-15

### Added

- Build signed packages from local recipes and guard package removal with its
  complete installed identity; report the App/Lib packages blocking removal.
- Allow anonymous read-only Workspace/Profile/capability discovery for new
  identities, with narrowly scoped standard-package install exemptions. Existing
  identities retain their access policies.
- Refine Workspace capability cards and align their heights.
- Deliver `pishoo-pack` with the native host for signed WEBC App/Lib packaging.
- Install, update and remove packages online with exact recursive Lib dependencies,
  persistent registration, signed expanded files and preserved private data.
- Add separate Workspace and Calendar Apps and Profile, Message and Calendar Libs
  under `std/`; publish and install these business packages independently.
- Expose the installed Lib access groups through native `GET /std/acl/permissions`.
- Export the identity 0.2.0 guest API through pishoo-sdk 0.1.0-preview.2.

### Changed

- Native services and business APIs share `/std` by actual path and method;
  management uses `/std/sys/settings`, `/std/sys/proxies` and `/std/sys/package`.
- Apply enabled/listen/proxy settings and discover identities at runtime.
  Linux root services discover the global and system accounts' identity homes.
- Keep host DEB/RPM/Homebrew packages independent of business WEBCs and gmutils;
  first startup initializes only native configuration and authorization.
- Load and query installed packages from their registration and existing resources,
  avoiding repeated installation validation on reads and startup.
- Split the Journal example into independently packaged App/Lib artifacts and
  request contact capabilities from the actual installed catalog.
- Expand verified packages into `apps/<name>` / `libs/<name>` and `std` subdirectories
  for the signed standard publisher; preserve registered legacy package reads.
- Use daccess and contact state for Message authorization; remove private approval
  tables and routes. Existing business data is retained without automatic migration.
- Reuse Wasmtime's persistent compilation cache and optimize compiler dependencies
  in development builds, with ordinary compilation as the cache-failure fallback.

### Fixed

- Submit offered contact capabilities through the native API, pin the verified
  peer before applying the owner's selected grants, and queue the friend request.
  Keep browser submission to one request so a first-time contact can offer access.
- Accept the store's empty App host metadata projection while rejecting forged
  capabilities and preserving signed Lib metadata checks.

### Dependencies

- Require dhttp 0.7.1, dhttp-home 0.6.1 and daccess 0.1.1 from crates.io;
  read OCSP caches consistently from `ssl/stapling.ocsp`.
- Remove sibling Cargo patches. Final registry lockfile refresh and release checks
  await these upstream publications.

### Release versions

| Package | Previous published version | Prepared version |
| --- | --- | --- |
| `pishoo` | 0.9.0-preview.1 | 0.9.0-preview.2 |
| `pishoo-common` | 0.9.0-preview.1 | 0.9.0-preview.2 |
| `pishoo-sdk` | 0.1.0-preview.1 | 0.1.0-preview.2 |

## [0.9.0-preview.1] - 2026-10-12

### Changed

- Replace the legacy gateway implementation with the frozen Server/Router/Sandbox/Lib design.
- Serve Workspace, access management, chat, files and Lib APIs under /std.
- Use SQLite configuration, startup initialization and explicit management commands.
- Bridge local HTTP/TCP and WebSocket upstreams; execute WASI HTTP components.
- Add Note and Journal examples and the guest identity SDK.
- Use daccess for authorization; remove legacy SSH and host exec.

### Release versions

| Package | Previous published version | Prepared version |
| --- | --- | --- |
| `pishoo` | 0.8.2 | 0.9.0-preview.1 |
| `pishoo-common` | 0.8.2 | 0.9.0-preview.1 |
| `pishoo-sdk` | first release | 0.1.0-preview.1 |


## [0.8.2] - 2026-08-31

### Added

- Add the server-scoped `sshd on;` directive and reserve
  `/shell/<username>` for SSH sessions when it is enabled.

### Changed

- Promote gateway, pishoo, and pishoo-common to stable 0.8.2 releases.
- Keep SSH disabled by default, route SSH requests through the server's
  existing `access_rules` policy, and always reject root login.
- Keep Pishoo connections alive for two minutes after effective traffic, using
  20-second heartbeat PINGs and a 60-second QUIC idle timeout.
- Split the global `pishoo.conf` example from the per-identity `server.conf`
  example.

### Fixed

- Tear down SSH sessions and child processes when the transport closes.
- Run PAM session setup on the process leader for `pam_loginuid`
  compatibility.

### Dependencies

- Require stable `dhttp` v0.6.2, `dshell` v0.6.2, and `h3x` v0.6.2.

### Components

- `pishoo` v0.8.2
- `gateway` v0.8.2
- `pishoo-common` v0.8.2 (`0.8.2-1` for DEB/RPM)

## [0.8.2-beta.2] - 2026-08-24

### Added

- Add the server-scoped `sshd on;` directive and reserve `/shell/<username>`
  for SSH sessions when it is enabled.

### Changed

- Keep SSH disabled by default and remove the legacy location-scoped
  `ssh_login`, `ssh_ssl_user`, and `ssh_deny` directives.
- Route SSH requests through the server's existing `access_rules` policy and
  always reject root login.
- Split the global `pishoo.conf` example from the per-identity `server.conf`
  example.
- Keep Pishoo connections alive for two minutes after effective traffic, using
  20-second heartbeat PINGs and a 60-second QUIC idle timeout so idle SSH
  sessions are not disconnected prematurely.

### Dependencies

- Require stable `dhttp` v0.6.2, `dshell` v0.6.2, and `h3x` v0.6.2.

### Components

- `pishoo` v0.8.2-beta.2
- `gateway` v0.8.2-beta.2
- `pishoo-common` v0.8.2-beta.2 (`0.8.2~beta.2-1` for DEB/RPM)

## [0.8.2-beta.1] - 2026-08-19

### Fixed

- Tear down SSH sessions and child processes when the transport closes.
- Run PAM session setup on the process leader for `pam_loginuid` compatibility.

### Dependencies

- Restore crates.io dependencies on `dhttp` v0.6.2 and `dshell` v0.6.2.

### Components

- `pishoo` v0.8.2-beta.1
- `gateway` v0.8.2-beta.1
- `pishoo-common` v0.8.2-beta.1 (`0.8.2~beta.1-1` for DEB/RPM)

## [0.8.1] - 2026-08-12

### Changed

- Promote gateway, pishoo, and pishoo-common to the stable 0.8.1 release.
- Require every release build path to use the committed Cargo lockfile.

### Dependencies

- Release manifests target stable `dhttp` v0.6.1 and `h3x` v0.6.1.

### Components

- `pishoo` v0.8.1
- `gateway` v0.8.1
- `pishoo-common` v0.8.1 (`0.8.1-1` for DEB/RPM)

## [0.8.1-beta.2] - 2026-08-11

### Fixed

- Reuse the network-owned mDNS resolver and driver across endpoint identities,
  avoiding duplicate multicast bindings and publication loops.
- Adapt fixed resolvers to the latest dquic lookup contract, including service
  names and address-family constraints.

### Dependencies

- Release manifests target `dhttp` v0.6.1 and `h3x` v0.6.1, which carry the
  dquic v0.7.1 bounded ACK-journal fix.

### Components

- `pishoo` v0.8.1-beta.2
- `gateway` v0.8.1-beta.2
- `pishoo-common` v0.8.1-beta.2 (`0.8.1~beta.2-1` for DEB/RPM)

## [0.8.1-beta.1] - 2026-08-07

### Added

- Reload active TLS identities when certificate files change, including
  coordinated updates across the root and worker control planes.

### Changed

- Identity watchers now filter certificate changes precisely and preserve the
  active listener while the replacement is applied.

### Dependencies

- Release manifests target `dhttp` v0.6.1-beta.2 and `h3x` v0.6.1-beta.1.

### Components

- `pishoo` v0.8.1-beta.1
- `gateway` v0.8.1-beta.1
- `pishoo-common` v0.8.1-beta.1 (`0.8.1~beta.1-1` for DEB/RPM)

## [0.8.0] - 2026-08-04

### Changed

- Promote gateway, pishoo, and pishoo-common to the stable 0.8.0 release.
- Align transport dependencies with dhttp 0.6.0, h3x 0.6.0, and dshell 0.6.0.
- Adapt STUN publication to one STUN client per interface.
- Write the authenticated short DHTTP identity in access logs instead of a
  transport IP address, reusing the per-connection identity cache.

### Components

- `pishoo` v0.8.0
- `gateway` v0.8.0
- `pishoo-common` v0.8.0 (`0.8.0-1` for DEB/RPM)

## [0.8.0-beta.7] - 2026-07-28

### Added

- Pishoo reverse proxy now supports WebSocket connections over HTTP/3 extended
  CONNECT, tunneling frames to HTTP/1.1 or HTTPS upstream WebSocket servers.

### Fixed

- Linux DEB/RPM installation hooks now create the `dhttp` group only when it is
  absent, and propagate lookup or group-creation failures instead of masking
  them. The DEB package now declares its required `adduser` dependency.

### Components

- `pishoo` v0.8.0-beta.7
- `gateway` v0.8.0-beta.5 (unchanged)
- `pishoo-common` v0.8.0-beta.7 (`0.8.0~beta.7-1` for DEB/RPM)

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

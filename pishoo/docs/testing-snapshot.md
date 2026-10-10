# Pishoo 同事测试依赖快照 — 2026-10-10

工作分支为 `feat/pishoo-integration`。项目依赖使用完整 Git revision，其他 Rust
依赖使用根 `Cargo.lock`，Workspace 前端使用 `pishoo/workspace/bun.lock`。
Cargo 自动下载依赖，不需要相邻仓库；分支继续变化不会改变本快照。

## 分支与提交

| 仓库 | 发布分支 | 固定提交 | 本次内容 |
| --- | --- | --- | --- |
| [dhttp](https://github.com/genmeta/dhttp/tree/fix/client-ocsp-compatibility) | `fix/client-ocsp-compatibility` | `03dad9b191a23bf93cdfd4cde9b4b56aed7938de` | 平台辅助网卡过滤；入站/出站在构造 H3 前装配失效连接移出池回调；固定 DQUIC/H3X |
| [dquic](https://github.com/genmeta/dquic/tree/fix/endpoint-kind-matching) | `fix/endpoint-kind-matching` | `b9af55da49c2e10a6b2004b72a90b0fff81da0ce` | 当前传输实现；严格 OCSP、端点种类匹配、路径/CID 分配及并发 Initial 去重 |
| [ddns](https://github.com/genmeta/ddns/tree/chore/h3-dns-diagnostics) | `chore/h3-dns-diagnostics` | `27663c4a1c1adf64ed1ea7bb79a63b582a347f9e` | 固定同一 DQUIC revision |
| [h3x](https://github.com/genmeta/h3x/tree/fix/qpack-feedback-backpressure) | `fix/qpack-feedback-backpressure` | `d1b475d8ae87da73ae886620ae702c978f75b868` | 连接池失效回调、并发建连协调与 GOAWAY；验证后的 peer SETTINGS 查询；固定同一 DQUIC |
| [daccess](https://github.com/genmeta/daccess/tree/feat/fit-pishoo-local) | `feat/fit-pishoo-local` | `3e3ac6199e88f6b5e521e8d29c934ead3bb5db7c` | 当前 `/std` 管理、审批及联系人协议；本次没有修改 |

本次对 dhttp、h3x、ddns 和 pishoo 按现有分支普通 push 发布，未重写其远端历史。
DQUIC 在整理期间由另一任务将3处测试修正合入并替换 HEAD，本表采用其最终发布提交。

## 依赖职责

| 依赖 | 职责与固定方式 |
| --- | --- |
| dhttp / dhttp-home | HTTP 请求、Endpoint、连接池、身份目录与凭据；共用 dhttp revision |
| DQUIC 的 qbase/qprotocol/qtls 等 crate | QUIC、地址维护、NAT、TLS 与身份；共用 dquic revision |
| ddns（包名 dyns） | H3 DDNS、mDNS 解析与发布 |
| h3x | HTTP/3 消息、流、QPACK、连接复用与 SETTINGS |
| daccess（包名 access_control） | 授权、审批与联系人管理 |
| Wasmtime / WASI / WASI HTTP | WASM component 执行；Cargo.lock 固定 47.0.4 |
| Axum / Tower / Hyper / Tokio | 路由、标准 Body、本机 HTTP/TCP 代理和异步 I/O；Cargo.lock 固定 |
| SeaORM / rusqlite | 配置、授权、Workspace/Chat 数据库；rusqlite 启用 bundled SQLite |
| Solid / Vite / TypeScript | 内嵌 Workspace 前端；bun.lock 固定 |
| qrustls | qtls 使用 crates.io 的 `=0.23.45`，由 Cargo.lock 固定 |

根清单的 crates.io patches 和直接 Git 依赖使用同一组 revision，避免来自不同源的
QUIC/H3 类型并存。xtask、工具和独立 WASM guest 项目各自维护锁文件，不参与宿主 workspace。
同仓库内部的 Cargo path（如 gateway、pishoo、dhttp/home、DQUIC 子 crate）保持正常使用。

## 构建与运行

本次验证：Rust 1.97.1、Bun 1.4.2、Node.js 23.11.0，macOS arm64。
需要原生 C/C++ 工具链（Linux 的 build-essential 或 macOS 的 Xcode Command Line Tools）。
首次构建需要访问 GitHub、crates.io 和前端包源；Bun 和 Node.js 必须在 PATH。

```sh
git clone --branch feat/pishoo-integration https://github.com/genmeta/pishoo.git
cd pishoo
cargo build --locked -p pishoo --bin pishoo
cargo test --locked --workspace -- --test-threads=1
cargo check --locked --workspace --all-targets
DHTTP_HOME=/path/to/identity-home ./target/debug/pishoo
```

根 `rust-toolchain.toml` 固定 Rust；Cargo 构建脚本执行
`bun install --frozen-lockfile` 和 TypeScript/Vite 生产构建并内嵌页面，不需要另起前端服务。
已提交 Echo/Info 组件；普通构建不需要 wasm-tools 或 WASI SDK。
Note 是独立 guest，相关测试默认跳过，另见 [Note 构建说明](../examples/note/README.md)。

复现前检查用户及祖先目录的 `.cargo/config.toml`：path patch 会覆盖本快照。
本次从父目录之外执行 Cargo，使用独立缓存，避开祖先目录的 path patches。
用 `cargo metadata --locked --format-version=1` 核对：只有本仓库 gateway、pishoo 的
`source` 为 null；上表项目包应来自固定 Git revision。

## 测试准备与兼容边界

使用独立 DHTTP_HOME 和有效身份凭据，不提交证书私钥、数据库或运行日志。
启动加载身份、配置和 Lib；更改后重启，OCSP 每72小时刷新。
当前 DQUIC 默认要求具名客户端和服务端提供有效 OCSP；匿名身份遵循原授权规则。
DDNS 缺少服务端 staple 时可用 `DQUIC_DDNS_OCSP_FILE` 提供同样需要验证的证明。

系统入口为 `/std`，包括 `/std/workspace/`、`/std/contact`、`/std/pishoo/*` 和
`/std/api/<LibId>/*`；旧 ACL 不自动迁移，双方需同步版本与规则，见
[系统路径说明](system-paths.md)。HA/OpenCode 仍使用其原生代理路径。

快速本机 QUIC smoke（单独进程、自动生成临时 CA/证书/OCSP，无需日常身份）：

```sh
# macOS 若默认 openssl 是 LibreSSL，先设置：
# export DHTTP_TEST_OPENSSL=/opt/homebrew/bin/openssl
cargo test --locked -p pishoo --lib transport_multithread_connectivity -- --ignored --test-threads=1
```

显式隔离传输验收：

```sh
# 首次先运行正常构建，缓存依赖；以下驱动使用 --offline。
python3 pishoo/tests/run-transport.py --soak-seconds 1800
python3 pishoo/tests/run-transport.py --case process
```

需要 Python 3、OpenSSL 3、本机 TCP/UDP 权限和约1.3 GiB测试文件空间；
用 `DHTTP_TEST_OPENSSL` 指定 OpenSSL 3。驱动把日志、结果、资源采样放在各自
`target/transport-*`。网络测试拥有进程全局 TLS roots、解析器和 DHTTP_HOME，必须单独运行。
[本机传输报告](transport-testing-2026-10-09.md)及[跨设备/NAT报告](transport-public-2026-10-09.md)
记录的是各自的历史版本与条件；包含失败项，不能当作本快照全通过或性能保证。

## 本次验证

- `cargo build --locked -p pishoo --bin pishoo` 通过，包含 Bun 1.4.2 的冻结安装及前端构建。
- `cargo test --locked --workspace -- --test-threads=1`：132 项通过，17 项按已有条件跳过。
- 在独立进程显式运行本机 QUIC 连通、真实 H3 WebSocket、Workspace/Chat 双向投递与发送前身份校验、早响应后上传取消，4项全部通过。
- `cargo check --locked --workspace --all-targets` 通过；nightly rustfmt、diff、Python语法和报告JSON检查通过。
- metadata 确认只有 gateway/pishoo 来自本地，17个项目依赖包全部使用上表固定 Git 来源；没有升级 registry 包版本。
- 整理依赖前后的对应源码回归：h3x 186项通过；dhttp 71项通过、2项显式跳过；DQUIC 路径11项通过。最后一轮清单仅调整最终Git哈希，DQUIC对应变化为3处测试借用写法修正，生产源码与回归时相同。

本次未重跑30分钟负载、完整大文件矩阵或跨设备/NAT。
本地 HAR、compat.zip、凭据、数据库、构建缓存及原始日志不进入提交。

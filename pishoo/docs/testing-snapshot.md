# Pishoo 同事测试依赖快照 — 2026-10-09

Pishoo 工作分支为 `feat/pishoo-integration`。以下依赖提交已发布到各自原工作分支；
Cargo 清单使用完整 `rev` 固定内容，不跟随分支后续变化。无需检出相邻仓库。

## 分支与提交

| 仓库 | 发布分支 | 固定提交 | 来源与变更 |
| --- | --- | --- | --- |
| [dhttp](https://github.com/genmeta/dhttp/tree/fix/client-ocsp-compatibility) | `fix/client-ocsp-compatibility` | `f8bdd2700e42782e5b68b62cf69827d37a1e305b` | 基于 `0a146494`，收录本地新 DQUIC 适配；Endpoint 持内存 identity，固定传输依赖 |
| [dquic](https://github.com/genmeta/dquic/tree/fix/endpoint-kind-matching) | `fix/endpoint-kind-matching` | `34a713b48535892ad9843cf2bb0db285e0d09439` | 直接发布当前本地 HEAD；包含新 QUIC/TLS 接口与 endpoint kind 匹配修复 |
| [ddns](https://github.com/genmeta/ddns/tree/chore/h3-dns-diagnostics) | `chore/h3-dns-diagnostics` | `b3ef11233e2598a75b316df104b35c2863f64e9f` | 基于 `65487e4b`，仅将 QUIC 依赖固定到当前 DQUIC |
| [h3x](https://github.com/genmeta/h3x/tree/fix/qpack-feedback-backpressure) | `fix/qpack-feedback-backpressure` | `c26abcd3d8596c10c0fadece240a713741c7873a` | 基于 `cd1ed3ae` 的 QPACK 修复，仅固定 qbase/qrecovery 来源；结构、接口与代码不变 |
| [daccess](https://github.com/genmeta/daccess/tree/feat/fit-pishoo-local) | `feat/fit-pishoo-local` | `3e3ac6199e88f6b5e521e8d29c934ead3bb5db7c` | 基于 `cf8f72f4`，收录本地 `/std` 管理路由、审批状态 URL、拉黑及管理员保护迁移 |

DQUIC 的原分支已重放到新上游，本次以验证过的 `34a713b4` 更新
`fix/endpoint-kind-matching`，使用绑定旧远端 `4acdce04` 的 force-with-lease。
DHTTP 和 daccess 提交包含此前的本地适配。临时 `feat/pishoo-test-20261009` 分支已删除。

## 依赖职责

| 依赖 | 职责与固定方式 |
| --- | --- |
| dhttp / dhttp-home | HTTP 请求、Endpoint、连接池、身份目录与凭据；共用上述 dhttp revision |
| DQUIC 的 qbase/qprotocol/qtls 等 crate | QUIC、地址维护、NAT、TLS 与身份；全部共用上述 dquic revision |
| ddns（Cargo 包名 dyns） | H3 DDNS 与 mDNS 解析、发布；使用上述 ddns revision |
| h3x | HTTP/3 消息、流与 QPACK；使用上述 h3x revision |
| daccess（Cargo 包名 access_control） | 授权、审批与联系人管理；使用上述 daccess revision |
| Wasmtime / WASI / WASI HTTP | WASM component 执行；Cargo.lock 固定 47.0.4 |
| Axum / Tower / Hyper / Tokio | 路由、标准 Body、本机 HTTP/TCP 代理与异步 I/O；由 Cargo.lock 固定 |
| SeaORM / rusqlite | 身份内的配置、授权及 Workspace/Chat 数据库；rusqlite 启用 bundled SQLite |
| Solid / Vite / TypeScript | 内嵌 Workspace 前端；由 pishoo/workspace/bun.lock 固定 |
| qrustls | 新 qtls 使用 crates.io 的 `=0.23.45`，由 Cargo.lock 固定；不再使用旧 rustls Git fork |

根 Cargo.toml 的 crates.io patches 与直接 Git 依赖使用相同 revision，
保证 DDNS、DHTTP、H3X 和 QUIC 共享兼容类型。其他 registry 依赖由 Cargo.lock 固定。

## 构建与运行

本次验证工具：Rust 1.97.1、Bun 1.4.2、Node.js 23.11.0，macOS arm64。
还需原生 C/C++ 编译工具；Linux 安装 build-essential，macOS 安装 Xcode Command Line Tools。
首次构建需要访问 GitHub、crates.io 和前端包源。

```sh
git clone --branch feat/pishoo-integration https://github.com/genmeta/pishoo.git
cd pishoo
cargo build --locked -p pishoo --bin pishoo
cargo test --locked --workspace
cargo check --locked --workspace --all-targets
DHTTP_HOME=/path/to/identity-home ./target/debug/pishoo
```

Cargo 自动执行 `bun install --frozen-lockfile` 和 Workspace 的 TypeScript/Vite 构建。
Bun 与 Node.js 必须在 PATH；不需要另起前端开发服务器。

在开发机已有父目录 `.cargo/config.toml` 时，先检查其中是否有相邻目录的 path patch；
这些配置会覆盖发布清单。此次验证暂存了父目录补丁，所有构建产物写入本仓库 target/。
依赖图检查确认只有本仓库 gateway、pishoo 两个包来自本地，其余项目依赖来自上述 Git revision。

普通构建使用已提交的 Echo/Info 测试组件，不需要 wasm-tools 或 WASI SDK。
Note 是独立 guest 项目，相关测试默认跳过；需要时按 [Note 说明](../examples/note/README.md)
构建组件，再安装到测试身份。xtask 与 guest 项目各自维护 Cargo.lock，不参与宿主 workspace 构建。

## 测试准备与兼容边界

使用独立 DHTTP_HOME 和测试身份，准备有效证书、私钥及 OCSP。Pishoo 在启动时加载资源，
身份、配置、Lib 或证书/私钥变更后重启；OCSP 每72小时刷新。
新 DQUIC revision 要求具名客户端及服务端提交有效 OCSP，旧客户端缺少 staple 时无法具名认证。
匿名请求仍使用匿名授权规则。DDNS 若缺少服务端 staple，可通过 DQUIC_DDNS_OCSP_FILE
提供经过验证的证明；缺少有效证明会导致连接失败。

系统入口已统一到 `/std`，包括 `/std/workspace/`、`/std/contact`、
`/std/pishoo/*` 和 `/std/api/<LibId>/*`。旧数据库 ACL 不自动迁移；
测试双方需同步升级，并按 [系统路径迁移说明](system-paths.md) 核对规则。
HA/OpenCode 等本机代理继续使用其原生 `/api`、`/file` 等路径。

建议两台设备各使用一个测试身份，验证 Workspace 页面、联系人申请与审批、
双向聊天、授权/撤权，以及本机 HTTP 代理。联网测试需单独记录浏览器/客户端版本、
设备与 NAT 环境；单元测试结果不等于真实网络验收。

## 本次验证

- `cargo build --locked -p pishoo --bin pishoo` 通过。
- `cargo test --locked --workspace`：132 项通过，11 项按已有条件跳过。
  其中 library 125 项、集成测试7项；跳过项包含真实网络、mDNS、Note 组件及手工页面 QA。
- `cargo check --locked --workspace --all-targets` 通过。
- Rust 格式检查通过；Workspace 前端由 Cargo 自动构建通过。
- 上述测试使用已发布 Git 依赖，未使用相邻仓库 path 依赖。

本次没有重新执行跨设备/NAT 或旧浏览器兼容验收。历史 VM 实测见
[Workspace 验收资料](workspace/README.md)。本地 HAR、身份凭据、数据库、日志及 target/ 不纳入提交。

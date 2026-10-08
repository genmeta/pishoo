# 同事复现运行（Git 提交为准）

这份说明对应 `feat/rebase-daccess` 的开发版本，不是软件源中较早的 0.8.2 包。
交接时提供 **Pishoo 的完整 commit SHA**；同事 checkout 该提交，使用提交内的
`rust-toolchain.toml`、`Cargo.lock`、`pishoo/workspace/bun.lock` 和固定 Git revision 构建。
不需要克隆相邻仓库，也不需要开发机父目录的 `.cargo/config.toml`。

## 固定依赖

| 仓库 | 用途 | 固定提交 | 推送来源分支 |
| --- | --- | --- | --- |
| dhttp | Endpoint、Network、身份目录 | `d925b240986eaa2863acfaf3f4ad34509e5795e2` | `main` |
| dquic | QUIC、TLS、解析和 NAT | `c64cf762cbaab9f0b848cf744d4f7352b2514864` | `dev` |
| h3x | HTTP/3 消息和流 | `90ab46c8df5332139d6ffd9eab30daa61b05bbca` | `feat/wasm` |
| ddns（crate 名 dyns） | H3 DDNS、mDNS | `c633d9880747ecb8e6211c017f0b5e51a003b59c` | `refactor/public-api` |
| daccess | 授权、审批和管理 API | `cf8f72f4e6bedbd7c98648ffee31053cb509b395` | `feat/fit-pishoo` |
| genmeta/rustls | QUIC 所需 TLS 实现 | `22dec513c4ebdf89f113f46e100393cf18963aa6` | `qtls-0.23.31` |

表中的分支用于发布提交；构建按 SHA 取源码，不跟随分支移动。
根清单的 crates.io 补丁将 DDNS/H3 的间接依赖也指向同一组 Git 源，避免重复的
TLS/解析器/传输类型。第三方 Rust 包使用 `Cargo.lock`，前端使用 Bun 冻结锁文件。
`xtask/` 和 WASM 示例各有独立锁文件，不参与通常的服务构建。

## 提交和推送交接

本次先保存已有功能改动，再单独保存依赖固定与本说明：

- Pishoo `9bf277d`：精简启动生命周期、删除宿主 exec、同步 DNS 与测试。
- Pishoo `4446ec2`：每72小时更新 OCSP，证书/私钥变化仍需重启。
- dhttp `c4a820b`：保存 NAT 映射维护和有界上传改动；`d925b24`：固定跨仓库 Git 依赖。
- h3x `90ab46c`：保存有界流写入改动。
- ddns `c633d98`：保存中继 DNS 记录编码测试。

先推送依赖，最后推送 Pishoo。以下从原开发机的 Pishoo 目录执行：

```sh
git -C ../dhttp push origin main
git -C ../h3x push origin feat/wasm
git -C ../ddns push -u origin refactor/public-api

# 已核对远端 dev 就是锁定的 c64cf762；无需再建分支或重复推送。
git -C ../dquic ls-remote origin refs/heads/dev

git push origin feat/rebase-daccess
git rev-parse HEAD
```

把最后输出的完整 Pishoo SHA 发给同事。若远端有后续更新导致普通 push 被拒绝，
先整合或用新的 `feat/` 分支发布锁定提交；不要强推现有分支。
工作区里尚未提交的改动不会进入交接版本。后续更新依赖时，显式调整 revision、
更新锁文件并重新验收。

## 环境

本轮验证平台为 macOS Apple Silicon。macOS Intel 和 Linux 有项目发布支持，
本轮未执行这些平台的构建；Windows 原生运行尚无本轮验收结果。

必需：Git、Rust **1.97.1**、Bun **1.4.2**、C/C++ 编译工具、首次构建的联网能力。
运行本机真实 QUIC 测试还需要 **OpenSSL 3**，并允许本机 TCP/UDP。
预编译 Lib 已随仓库提交，通常构建不需要 `wasm-tools` 或 WASM target。

macOS：

```sh
xcode-select --install                 # 已安装 Command Line Tools 时跳过
brew install openssl@3
rustup toolchain install 1.97.1 --profile minimal --component rustfmt
curl -fsSL https://bun.sh/install | bash -s -- bun-v1.4.2
export PATH="$HOME/.bun/bin:$PATH"
export DHTTP_TEST_OPENSSL="$(brew --prefix openssl@3)/bin/openssl"
```

以上假设已有 Homebrew 和 rustup；`brew` 的 OpenSSL 小版本未锁定，测试要求其为 3.x。
Linux（Debian/Ubuntu）可用 `build-essential pkg-config openssl curl git unzip` 替代
Xcode/Homebrew，再安装相同版本 Rust/Bun。Linux 步骤是环境说明，尚未在本轮实测。

## 克隆、构建和本机验收

```sh
git clone --branch feat/rebase-daccess https://github.com/genmeta/pishoo.git
cd pishoo
# 用交接人给出的完整 SHA 替换占位值：
git checkout --detach <PISHOO_COMMIT_SHA>

rustc --version                       # 1.97.1
bun --version                         # 1.4.2
openssl version                       # 或 "$DHTTP_TEST_OPENSSL" version
cp .env.example .env
set -a
. ./.env
set +a

cargo build --locked -p pishoo --bin pishoo --example pishoo-client
cargo test --locked --workspace -- --test-threads=1
```

Cargo 的 build.rs 会执行 `bun install --frozen-lockfile` 和 `bun run build`，
把 Workspace 页面内嵌进二进制；不用另开 Vite 服务。
`.env` 不会自动加载，其中根证书、DDNS/bootstrap/mDNS 参数用于编译期配置；
修改这些值后重新执行 Cargo 构建，再重启。
不要使用 `cargo update` 或重新生成前端锁文件来解决环境错误。
首次拉取依赖和前端包需要网络，不承诺离线安装。

开发机的上级目录若有 `.cargo/config.toml` 中的本地 path 补丁，会覆盖仓库清单，
导致 `--locked` 报错。复现请使用不在该目录下的全新 checkout；不要为此重写锁文件。

不需要申请线上身份的真实本机 HTTP/3 验收：

```sh
cargo test --locked -p pishoo --lib \
  config_api_real_h3_persistence_authorization_and_restart \
  -- --ignored --test-threads=1 --nocapture

cargo test --locked -p pishoo --lib \
  workspace_chat_real_quic_delivery_and_pre_send_identity_check \
  -- --ignored --test-threads=1 --nocapture
```

每条命令单独运行：测试拥有进程级 TLS/DNS 状态，生成临时证书、有效 OCSP 和隔离
数据库，实际完成本机 QUIC/H3 请求。前者检查配置读写、授权和重启后的持久性；
后者检查联系人/Chat 投递和发送前身份核对。测试结束自动清理临时身份数据。
普通 `cargo test` 会跳过这些显式验收项。旧 `h3x-tcp-smoke.sh` 和
`setup-tcp-demo` 使用已撤下的 TCP mock 素材，不是这次的运行入口。

## 长期启动服务

长期服务需要有效的 DHTTP 身份证书和私钥；源码提交不包含同事的身份凭据。
请为同事使用独立身份，不拷贝开发机的私钥或日常数据库。
将已申请的凭据放入独立 home，例如：

```text
~/pishoo-home/
  colleague/
    ssl/
      fullchain.crt
      privkey.pem
      ocsp.der          # 有效缓存；缺失时启动会尝试获取
```

目录 `colleague` 对应 `colleague.dhttp.net`；证书名称和 owner 身份必须匹配。
然后从构建后的仓库运行：

```sh
export DHTTP_HOME="$HOME/pishoo-home"
export PISHOO_CLIENT_IDENTITY="colleague.dhttp.net"
chmod 400 "$DHTTP_HOME/colleague/ssl/privkey.pem"
./target/debug/pishoo
```

第一次启动在该身份目录创建数据库及资源目录，默认 `listen=3`，允许 LAN 和公网
发布。可以在另一个终端用同一身份确认服务可访问：

```sh
export DHTTP_HOME="$HOME/pishoo-home"
export PISHOO_CLIENT_IDENTITY="colleague.dhttp.net"
./target/debug/examples/pishoo-client get /sys/settings
./target/debug/examples/pishoo-client get /workspace/
```

预期日志为 `HTTP/3 200 OK`，分别返回 listen JSON 和 Workspace HTML。
Workspace 的日常浏览需使用支持 DHTTP 的客户端/浏览器；当前进程没有普通
HTTP `localhost:3000` 的 UI 监听入口。启动时被跳过的无效身份不会提供服务，
要核对跳过日志及证书有效期；零身份进程仍运行不代表服务验收通过。
真实线上运行另需能访问 DDNS、bootstrap 和证书 OCSP 服务，并允许 QUIC UDP。
本机验收通过不等于跨设备 NAT 已验收。

数据库配置位于 `<DHTTP_HOME>/<identity>/db/config.db`；没有 `server.conf`。
身份、代理配置和 Lib 变更通过重启生效，Ctrl-C 退出。
已加载身份的 OCSP 每72小时刷新；证书或私钥轮换仍需重启。
静态文件放 `file/`，访问路径 `/file/{path}`；Lib 放 `lib/<id>/lib.wasm`。
代理仅支持本机 HTTP/TCP 上游，Lib WASI HTTP 出站暂不开放。配置写入和授权规则
见 [配置 API](../pishoo/docs/config-api.md) 与 [daccess 集成](../pishoo/DACCESS.md)。

## 本轮验证记录

2026-10-08，在 macOS Apple Silicon、Rust 1.97.1、Bun 1.4.2、OpenSSL 3 下：

- 使用新的 Cargo Git 缓存直接从 GitHub 拉取全部固定 revision，依赖解析通过。
- 在仓库外的临时源码目录构建，服务二进制、客户端 example、内嵌前端构建通过。
- 常规 workspace 测试114项通过，5项显式环境测试默认跳过。
- 单独执行真实 HTTP/3 配置验收和 Workspace/Chat QUIC 验收，均通过。
- `git diff --check` 通过；依赖来源改为 Git，第三方包版本未升级。

依赖图中本地包只有 Pishoo/gateway，内部依赖全部来自固定 Git source。
未在本轮测试 Linux/macOS Intel、跨设备 NAT 或长期线上身份运行。

# 第一版实施记录

日期：2026-09-26。这是实现和验收记录，接口以 `design/README.md` 为准。

## 修改前的 Git 基线

| 仓库 | 分支 | 基线提交 |
| --- | --- | --- |
| Pishoo | main | `4b36f36` |
| dhttp | main | `a82d654` |
| h3x | feat/wasm | `fa5835c` |

实现已在基线之后单独保存阶段提交，后续文件整理与功能实现分开审阅。h3x 保持结构、字段和接口，只修复读取消息头期间取消 future 的原生流清理。

文件整理前的实现检查点：Pishoo `58fafc0`、dhttp `502ad17`、h3x `592989b`。

## 文件组织

测试代码统一放在各 crate 的 `tests/` 下：`unit/` 存放需要访问私有实现的单元测试，`support/` 存放内存流等测试工具，`cases/` 存放较长集成测试的分组。`src/` 仅保留测试模块挂载声明，不为测试扩大生产 API 的可见性。

Pishoo 的 daemon、routes、setup、terminal、wasm 按职责拆分到同名目录。实现片段通过 `include!` 保持冻结的 Rust 模块边界，避免增加跨模块 helper API；Workspace 页面放在 `pishoo/assets/`。

## 当前实现

- h3x：既有消息读取方法在等待 HEADERS 前后保留明确的取消所有权；裸流 Drop、正常 EOF 和半关闭约定保持原样。
- dhttp：独立 Endpoint、可 await 的标准请求、全局 Network、标准 Service 接入、原生流与标准 Body 适配、证书接缝、同名永久关闭、临时停止和重新监听。
- Pishoo：实例锁与 TOML 配置、schema v1 数据库读取、身份扫描、直接 Server/Router/Lib 所有权、串行重载、坏候选保留、删除时取消旧版本、按身份故障隔离。
- 路由：受目录能力约束的流式静态文件、精确/最长前缀代理、DHTTP 唯一出站、WASM 显式方法路由、当前 daccess 授权与请求内审批、管理 API 和最小 Workspace 查看/审批界面。
- WASM：单 Lib 的 `/data` 权限、实际 Store 内存/fuel 限制、4 个 Semaphore 执行槽、30 秒独立监督任务、流式响应、提前丢弃取消、身份签名与受限出站。
- 终端：配置、管理员准入、固定二进制帧编解码、输入半关闭、撤权与关闭流程。**没有执行后端**；默认关闭，启用且身份检查通过后返回 501。

用户确认的唯一字段变更：`ProxyLocation.proxy_pass` 改为 `http::uri::Parts`，保留未写路径和显式 `/` 的差异；已记录在冻结清单中。

## 本地配置

`DHTTP_HOME` 必须在进程启动前设置，且与 `state_dir` 指向同一目录。程序不修改进程环境。入口读取 `$DHTTP_HOME/pishoo.toml`：

```toml
state_dir = "/absolute/path/to/dhttp-home"

[terminal]
enabled = false
administrators = []
```

每个身份保持 `ssl/`，静态文件放在 `file/`，组件放在 `lib/<id>/lib.wasm`。组件顶层须有唯一的 `pishoo:openapi` 自定义段。

每个身份的 `db/config.db` 使用现有 schema v1：

```sql
PRAGMA user_version = 1;
CREATE TABLE settings (listen INTEGER NOT NULL);
INSERT INTO settings VALUES (1);
CREATE TABLE proxy_locations (location TEXT NOT NULL, proxy_pass TEXT NOT NULL);
```

listen 的 0/1/2/3 分别代表关闭/内网/外网/两者；改变监听范围需要重启。代理支持 `https://bob~/...` 和完整 DHTTP 名称。未写路径的上游保留原路径，显式写 `/` 的上游按匹配前缀替换路径。

## 验证

首批实现验证结果：Pishoo 47 项、dhttp 27 项、h3x 149 项通过。

文件整理后保持上述测试通过，并复验 dhttp 子库：home 31 项、identity 112 项、access 启用 migration/http 时 49 项及 20 项文档测试、log 12 项单元测试及 25 项集成测试。

在各仓库执行：

```sh
# Pishoo：标准内存 Body、真实 WASM Store、SQLite/daccess 与重载测试
cargo test --locked --offline --workspace
cargo check --locked --offline --workspace --all-targets
cargo fmt -p pishoo --check
# 同模块 include! 片段也需要直接格式检查
rg --files pishoo/src pishoo/tests/unit | rg '\.rs$' | xargs rustfmt --edition 2024 --check

# dhttp：h3x 内存双向流与监听生命周期测试
cargo test --locked --offline -p dhttp --lib --tests

# h3x：保持既有结构/接口的协议行为验证
cargo test --locked --offline --lib --test request_response --test stream_lifecycle --test wnd_cancel --test body_chunks --test transport_io_failure --test message_traits
```

流测试不要求真实 dquic 联网，覆盖提前响应、背压、多值 trailers、Body 取消和 HEAD/204/304。Pishoo 测试不持有 QPACK 或 H3 writer。

## 尚未完成的验收

- 当前 qconn 出站连接仍缺少实际路径发现；真实跨端请求尚不能据此宣称完成。这里保留其既有接口，以内存流验证上层通信行为。
- 终端 helper、文件 broker、Linux namespaces/FUSE/cgroup 后端、macOS WASI helper 和各平台隔离/后代回收验收仍待实现。不启动宿主 shell 作为替代。
- Workspace 当前提供列表查看和审批操作；完整联系人/规则编辑交互仍待完善。管理 API 已直接使用当前 daccess 库。

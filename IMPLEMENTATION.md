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

Sandbox 拆分前的 Pishoo 文件整理检查点：`a6a19be`。

WASM 职责集中到 Sandbox 前的 Pishoo 检查点：`bc30231`，保存上一轮 Sandbox 拆分。

## 文件组织

测试代码统一放在各 crate 的 `tests/` 下：`unit/` 存放需要访问私有实现的单元测试，`support/` 存放内存流等测试工具，`cases/` 存放较长集成测试的分组。`src/` 仅保留测试模块挂载声明，不为测试扩大生产 API 的可见性。

Pishoo 使用普通 `mod` 声明和同名 `.rs` 文件，子模块放在同名目录。库入口分别为 `pishoo/src/pishoo.rs` 和 `gateway/src/gateway.rs`，由 Cargo 的 `[lib].path` 指定。Sandbox 合并为四个文件：`sandbox.rs` 负责组件管理和 API 路由，`sandbox/runtime.rs` 负责编译、Store、执行和响应体，`sandbox/host.rs` 负责出站和身份宿主能力，`sandbox/manifest.rs` 负责清单校验。测试继续放在 `tests/unit/`，通过 `#[path] mod` 挂载；测试文件也使用同名 `.rs`，不使用 `mod.rs`。Workspace 页面放在 `pishoo/assets/`。

Pishoo 的其他实现按同样原则合并：`daemon.rs` 集中守护循环，`daemon/server.rs` 负责单身份服务；`routes.rs` 集中分发与静态文件，`routes/access.rs` 负责授权和管理入口，`routes/proxy.rs` 负责反代；配置归 `setup.rs`，单命令宿主执行归 `exec.rs`。

h3x 同样合并同类型实现：帧载荷收拢为 `frame/payload.rs`，SETTINGS 构造并入 connection，QPACK 编码状态与算法并入 encoder，请求/响应读取并入 read。dhttp 的 Endpoint 收拢为入口/连接/服务、网络维护、消息与 Body 适配三个文件；identity、home、access 中同职责的实现片段合并。两仓测试片段合并回所属测试模块；库根与模块根采用同名 `.rs`。

## 当前实现

- h3x：既有消息读取方法在等待 HEADERS 前后保留明确的取消所有权；裸流 Drop、正常 EOF 和半关闭约定保持原样。
- dhttp：全局 Network 持有以本端、远端规范化名称为键的 h3x 连接池；Endpoint 只持名称，不提供 close。同名 load 复用连接，stop_listening 仅撤销当前监听。Network 没有 shutdown，进程退出时结束其剩余传输与维护任务。
- Pishoo：实例锁与 schema v1 数据库读取、身份扫描、Server 直接持有 Router 与 Sandbox、Sandbox 直接持有 Lib、串行重载、坏候选保留、删除时取消旧版本、按身份故障隔离。
- 路由：受目录能力约束的流式静态文件、精确/最长前缀代理、DHTTP 唯一出站、WASM 显式方法路由、当前 daccess 授权与请求内审批、管理 API 和最小 Workspace 查看/审批界面。
- WASM：每身份一个 Sandbox，持有 Lib 集合、共享 Runtime 引用和 WASM 任务跟踪器，集中组件与执行管理；单 Lib 的 `/data` 权限、实际 Store 内存/fuel 限制、30 秒独立监督任务、流式响应、提前丢弃取消、身份签名与受限出站。
- exec：每 Server 的 `settings.ssh`、与 Lib 合并的 `POST /exec` Router 分支、daccess 加同名身份准入、直接 argv、固定并发与输入输出限制、受跟踪的 Child 取消和回收。程序使用 Pishoo 当前非 root 服务账号权限，没有文件或网络隔离。

原交互终端的安装布局探测、WASM shell WIT、帧协议和平台 helper 方案已按用户的新 v1 决定移除。单命令 exec 的标准输入输出采用有界 JSON/base64；不使用 PTY、shell 解释或子进程 IPC。

用户明确确认的契约变更均已记录在冻结清单中：

- `ProxyLocation.proxy_pass` 改为 `http::uri::Parts`，保留未写路径和显式 `/` 的差异。
- Sandbox 集中持有 `libs: BTreeMap<String, Arc<Lib>>`、`runtime: Arc<Runtime>`、`tasks: TaskTracker`；Server 删除独立的 Lib 集合和 Runtime 引用，直接持有 `sandbox: Sandbox`。`run` 直接创建并共享 Runtime。
- 移除 DaemonConfig、Daemon、TerminalPolicy 和实例配置文件；`run` 的局部变量负责身份扫描、监听、串行重载与关闭，每个 Server 直接读取自己的 `db/config.db` 并持有 exec 任务跟踪器和专用名额。
- Sandbox 构造接收 Runtime，新增 `load_libs/verify_libs/replace_libs/api_router`，同步 `close(&mut self)` 关闭准入、取消并清空 Lib；`wait(&self) -> Result<()>` 保持不变。`build_router` 接收已构造的标准 Lib Router，WASM 内部类型和 manifest 验证统一归 sandbox，`validate_lib` 的根级公开导出不变。

Sandbox 的关闭分为停止准入、清除 Lib 与等待任务回收；身份取消仍由 Server 的根 token 负责。Server 继续直接持有 Endpoint、授权、完整 Router 与 exec 资源，Lib 不限制并发数，删除 `Sandbox.lib_slots`、`Invocation.permit`、`StoreData.permit`、`Invocation::new` 的 permit 参数及 `Error::Capacity`；保留单次执行的内存、fuel、出站次数和超时限制。不新增内部锁、取消信号、派生计数或通用策略容器，也不把 Sandbox 描述为操作系统进程或容器隔离。

重载先在局部加载候选，由 Sandbox 构造 Lib Router，再装配完整 Router；目录和组件摘要全部复核成功后，同步替换完整 Router、Sandbox 的 Lib 集合与配置。Router 装配或复核失败均保留旧版本，也不会提前取消待删除 Lib。

## 本地配置

`DHTTP_HOME` 必须在进程启动前设置。程序不修改进程环境；没有实例配置文件或实例数据库。每个 Server 只读取自己的 `db/config.db`。

每个身份保持 `ssl/`，静态文件放在 `file/`，组件放在 `lib/<id>/lib.wasm`。组件顶层须有唯一的 `pishoo:openapi` 自定义段。

每个身份的 `db/config.db` 使用现有 schema v1：

```sql
PRAGMA user_version = 1;
CREATE TABLE settings (listen INTEGER NOT NULL, ssh INTEGER NOT NULL);
INSERT INTO settings VALUES (1, 0);
CREATE TABLE proxy_locations (location TEXT NOT NULL, proxy_pass TEXT NOT NULL);
```

listen 的 0/1/2/3 分别代表关闭/内网/外网/两者；ssh 只能为 0/1，作为本 Server 单命令 exec 的开关，只开放给同名已验证远端身份。改变 listen 或 ssh 需要重启。代理支持 `https://bob~/...` 和完整 DHTTP 名称。未写路径的上游保留原路径，显式写 `/` 的上游按匹配前缀替换路径。

## 验证

首批实现验证结果：Pishoo 47 项、dhttp 27 项、h3x 149 项通过。

Sandbox 拆分后：Pishoo 50 项测试通过，覆盖不同身份的执行槽隔离、重载沿用同一 Sandbox、等待实际任务/permit 回收，以及关闭超时仍保留未完成任务的资源所有权。

WASM 职责集中到 Sandbox 后：Pishoo 56 项测试通过，编译与格式检查通过。新增验证覆盖候选摘要改变时保留旧版本与取消状态、Router 快照共享执行额度、HEAD/OPTIONS 显式声明、API 命名空间隔离，以及通过实际 WASM 执行验证 Sandbox 关闭回收。

取消 Lib 并发限制后：Pishoo 56 项测试通过，覆盖8个同时在途的实际 WASM 请求、跨版本6个在途执行的共同取消、关闭后旧 Router 拒绝执行，以及任务回收、内存/fuel 和超时约束。

改用普通 `mod`、库根文件改名及 Sandbox 四文件合并后：Pishoo 56 项测试、工作区全部目标编译检查和 `cargo fmt --all --check` 通过。

扩展到三仓的模块合并后：h3x 162 项测试通过；dhttp 工作区启用 access 的 migration/http 特性后，256 项测试及20项文档测试通过。两仓在临时副本使用 stable 完成全量测试，应用补丁后逐文件确认源码与测试副本相同；Pishoo 再使用原仓库 nightly 工具链运行56项测试，验证实际三仓依赖链。三仓 nightly 格式检查通过。

文件整理后保持上述测试通过，并复验 dhttp 子库：home 31 项、identity 112 项、access 启用 migration/http 时 49 项及 20 项文档测试、log 12 项单元测试及 25 项集成测试。

单命令 exec 替代交互终端、同步当前 Network 生命周期后：Pishoo 49 项单元测试及工作区文档测试通过，工作区全部目标编译检查通过。exec 测试覆盖正常输出与退出码、主动取消后的直接子进程回收、无限输出触发上限并结束进程、分片内存 Body 请求得到 JSON 结果、待续传 Body 遇 Server 取消时立即结束，以及 `/exec` 在应用 Router 中挂载。可信远端身份的正向路径尚未在真实跨端请求中验证；主动脱离进程组的后代不在本版保证内。

在各仓库执行：

```sh
# Pishoo：标准内存 Body、真实 WASM Store、SQLite/daccess 与重载测试
cargo test --locked --offline --workspace
cargo check --locked --offline --workspace --all-targets
cargo fmt --all --check

# dhttp：h3x 内存双向流与监听生命周期测试
cargo test --locked --offline -p dhttp --lib --tests

# h3x：保持既有结构/接口的协议行为验证
cargo test --locked --offline --lib --test request_response --test stream_lifecycle --test wnd_cancel --test body_chunks --test transport_io_failure --test message_traits
```

流测试不要求真实 dquic 联网，覆盖提前响应、背压、多值 trailers、Body 取消和 HEAD/204/304。Pishoo 测试不持有 QPACK 或 H3 writer。

## 尚未完成的验收

- 当前 qconn 出站连接仍缺少实际路径发现；真实跨端请求尚不能据此宣称完成。这里保留其既有接口，以内存流验证上层通信行为。
- exec 使用服务账号权限，不提供 OS 沙箱。主动脱离本次进程组的后代不在本版回收保证内；不宣称支持交互终端或任意恶意命令的完整资源隔离。
- Workspace 当前提供列表查看和审批操作；完整联系人/规则编辑交互仍待完善。管理 API 已直接使用当前 daccess 库。

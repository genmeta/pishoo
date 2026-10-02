# 第一版实施记录

日期：2026-09-26。这是实现和验收记录，接口以 `design/README.md` 为准。

## 2026-10-02：rebase daccess 与 Workspace/Chat

- 当前适配分支 `feat/rebase-daccess` 以 `origin/feat/daccess@9b733c5` 为基线，重放14个本地提交。重放前的完整工作保存在 `feat/pre-daccess-rebase-20261002@33f2d50`；原 main 保持原提交。range-diff 确认其余13个补丁相同，首个重构提交仅调整与远端旧架构文件的冲突。
- daccess 固定 Git revision `cf8f72f4e6bedbd7c98648ffee31053cb509b395`。审批改为202、持久记录和按 Visitor 查询；联系人改为申请队列及 application_id 轮询，ContactNotifier 回调接缝删除。
- 完整 Workspace 前端保留在 `pishoo/workspace`，应用页面源码、样式和锁文件与目标分支一致。`src/workspace/mod.rs`、`src/chat/mod.rs` 改名为普通 `workspace.rs`、`chat.rs`，旧占位 HTML 由 dist 资源替代。
- Server 直接拥有 Workspace/Chat，重载复用资源，关闭时停止并等待现有 worker。保留本地 Sandbox、Lib、exec、配置和本机反代实现；上述生产文件与 rebase 前快照一致。
- 修正目标分支已有的测试导入、身份 fixture、过期时间和计时精度问题；前端 E2E 补齐当前目录 API 的 mock，并按当前审批详情抽屉和 Allow 动作校准预期。

验证：

- Rust 集成编译通过。完整库测试92项首次90项通过，随后按目标库补齐审批 Visitor 及缺失身份的状态码预期，6项授权/联系人测试复测全部通过；其余86项无需变更。
- 前端 `bun run build` 与 `bun run typecheck` 通过，验证使用临时 Bun 1.4.2。
- 桌面 mock E2E：21项通过，1项移动端专用用例按配置跳过。浏览器使用独立临时配置下的本机 Chrome；不使用日常浏览器 profile。
- `git diff --check` 与 Rust 格式检查通过。

限制：Rust 检查使用临时清单，仅将 `tcp-mock = ["dhttp/tcp-mock"]` 改为空 feature，实际生产源码与其他依赖不变。正式清单仍保留这一已知底层 feature 冲突，等待用户要求的底层接口稳定后统一处理。当前 Workspace/Chat 的生产 OutboundTransport 尚未配置；远端资料返回503，申请与消息保留在数据库队列等待接入。原网络测试保存于 `pishoo/tests/deferred/workspace_network.rs`。本次未修改相邻 daccess、dhttp、h3x 的源码或日常 profile 数据库，不宣称真实跨端出站已验收。

当前接口以 [Pishoo 清单](design/pishoo-interfaces.md) 和 [Workspace/Chat 清单](design/workspace-chat-interfaces.md) 为准。以下保留此前各轮的实施记录。

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

Pishoo 使用普通 `mod` 声明和同名 `.rs` 文件，子模块放在同名目录。库入口分别为 `pishoo/src/pishoo.rs` 和 `gateway/src/gateway.rs`，由 Cargo 的 `[lib].path` 指定。Sandbox 合并为四个文件：`sandbox.rs` 负责组件管理和 API 路由，`sandbox/runtime.rs` 负责编译、Store、执行和响应体，`sandbox/host.rs` 负责 WASI HTTP 出站拒绝接缝和身份宿主能力，`sandbox/manifest.rs` 负责清单校验。测试继续放在 `tests/unit/`，通过 `#[path] mod` 挂载；测试文件也使用同名 `.rs`，不使用 `mod.rs`。Workspace 前端现位于 `pishoo/workspace/`，构建后内嵌 dist 资源。此次接入保留 Workspace/Chat 分支原有的业务测试布局。

Pishoo 的其他实现按同样原则合并：`server.rs` 集中运行循环和单身份服务；`routes.rs` 集中分发与静态文件，`routes/access.rs` 负责授权和管理入口，`routes/proxy.rs` 负责反代；配置归 `setup.rs`，单命令宿主执行归 `exec.rs`。

h3x 同样合并同类型实现：帧载荷收拢为 `frame/payload.rs`，SETTINGS 构造并入 connection，QPACK 编码状态与算法并入 encoder，请求/响应读取并入 read。dhttp 的 Endpoint 收拢为入口/连接/服务、网络维护、消息与 Body 适配三个文件；identity、home、access 中同职责的实现片段合并。两仓测试片段合并回所属测试模块；库根与模块根采用同名 `.rs`。

## 当前实现

- h3x：既有消息读取方法在等待 HEADERS 前后保留明确的取消所有权；裸流 Drop、正常 EOF 和半关闭约定保持原样。
- dhttp：全局 Network 持有以本端、远端规范化名称为键的 h3x 连接池；Endpoint 只持名称，不提供 close 或 stop_listening。同名 load 复用连接。Network 没有 shutdown，进程退出时结束其剩余传输与维护任务。
- dhttp 操作等待：删除 `OPERATION_TIMEOUT` 及开流、消息头和 Body 读写的单次超时；保留连接超时与流背压。出站请求 future 或响应 Body 提前丢弃时，现成 scopeguard 中止尚未结束的上传任务。
- Pishoo：schema v1 数据库读取、启动时扫描身份及 SIGHUP 显式重载、Server 直接持有 Router 与 Sandbox、Sandbox 直接持有 Lib、串行重载、加载失败直接返回、删除时撤销入口。监听任务不保留句柄。
- 路由：受目录能力约束的流式静态文件、精确/最长前缀本机 HTTP/TCP 代理、同名身份专用的固定前缀 DHTTP 正向代理、WASM 显式方法路由、daccess 授权与202持久审批、管理 API、Workspace 联系人申请队列与轮询，以及完整 Workspace/Chat 界面。生产出站接缝暂缓。Lib 的 WASI HTTP 出站暂不实现。
- WASM：每身份一个 Sandbox，持有 Lib 集合、共享 WasmRuntime 引用和 WASM 任务跟踪器，集中组件与执行管理；单 Lib 的 `/data` 权限、实际 Store 内存/fuel 限制、无总时长上限的受跟踪 guest 任务、流式响应、身份签名与出站拒绝 hook。
- exec：每 Server 的 `settings.exec`、与 Lib 合并的 `POST /exec` Router 分支、daccess 加同名身份准入、直接 argv、输入输出和单次执行限制、受跟踪的 Child 取消和回收。程序使用 Pishoo 当前非 root 服务账号权限，没有文件或网络隔离。

原交互终端的安装布局探测、WASM shell WIT、帧协议和平台 helper 方案已按用户的新 v1 决定移除。单命令 exec 的标准输入输出采用有界 JSON/base64；不使用 PTY、shell 解释或子进程 IPC。

用户明确确认的契约变更均已记录在冻结清单中：

- `ProxyLocation.proxy_pass` 改为 `http::uri::Parts`，保留未写路径和显式 `/` 的差异。
- Sandbox 集中持有 `libs: HashMap<String, Arc<Lib>>`、`runtime: Arc<WasmRuntime>`、`tasks: TaskTracker`；Server 删除独立的 Lib 集合和 WasmRuntime 引用，直接持有 `sandbox: Sandbox`。`run` 直接创建并共享 WasmRuntime。
- 移除 DaemonConfig、Daemon、TerminalPolicy 和实例配置文件；`run` 的局部变量负责身份扫描、监听、串行重载与关闭，每个 Server 直接读取自己的 `db/config.db` 并持有 exec 任务跟踪器。
- Sandbox 构造接收 WasmRuntime，`load_libs(&mut self)` 扫描成功后直接更新集合，`api_router` 从当前集合构建路由；同步 `close(&mut self)` 关闭任务登记并清空 Lib；`wait(&self) -> Result<()>` 保持不变。Server 显式合并管理、Lib API、exec 和 `/file/{*path}` 静态路由，设置代理 fallback 与统一授权层；WASM 内部类型和 manifest 验证统一归 sandbox，`validate_lib` 的根级公开导出不变。

Sandbox 的关闭分为停止准入、清除 Lib 与等待任务回收；在途 Lib 执行继续运行直到自行结束；关闭等待仍有15秒上限。Server 继续直接持有 Endpoint、授权、完整 Router 与 exec 任务跟踪器，Lib 和 exec 都不限制并发数，删除 `Sandbox.lib_slots`、`Invocation.permit`、`StoreData.permit`、`Invocation::new` 的 permit 参数及 `Error::Capacity`；保留单次执行的内存和 fuel 限制，不设 WASM 总执行期限。不新增内部锁、取消信号、派生计数或通用策略容器，也不把 Sandbox 描述为操作系统进程或容器隔离。

启动和重载都先让 Sandbox 扫描并加载 Lib，再构造 Lib Router 与完整 Router；扫描或编译失败直接返回，保留旧集合和路由。成功后同步替换完整 Router 与配置。Lib API 不自动写 daccess 规则；未匹配时使用 daccess 的默认策略，已有管理员规则保持原样。

## 本地配置

`DHTTP_HOME` 必须在进程启动前设置。程序不修改进程环境；没有实例配置文件或实例数据库。每个 Server 只读取自己的 `db/config.db`。

每个身份保持 `ssl/`，静态文件放在 `file/` 并通过 `/file/{*path}` 访问，组件放在 `lib/<id>/lib.wasm`。组件顶层须有唯一的 `pishoo:openapi` 自定义段。

每个身份的 `db/config.db` 使用现有 schema v1：

```sql
PRAGMA user_version = 1;
CREATE TABLE settings (listen INTEGER NOT NULL, exec INTEGER NOT NULL);
INSERT INTO settings VALUES (1, 0);
CREATE TABLE proxy_locations (location TEXT NOT NULL, proxy_pass TEXT NOT NULL);
```

listen 的 0/1/2/3 分别代表关闭/内网/外网/两者；exec 只能为 0/1，作为本 Server 单命令 exec 的开关，只开放给同名已验证远端身份。改变 listen 或 exec 需要重启。代理支持 `127.0.0.1:8080` 和 `http://127.0.0.1:8080[/路径]` 等回环 HTTP/TCP 上游。未写路径的上游保留原路径，显式写 `/` 的上游按匹配前缀替换路径。

## 验证

取消 WASM 总执行期限后：Pishoo 49 项库测试通过。模拟时间推进 31 秒的未轮询流式响应仍在执行，随后取消可回收；无限循环 guest 可被取消，未取消时因 fuel 耗尽而退出。`cargo fmt -p pishoo --check` 与 `git diff --check` 通过。

2026-09-27 TCP mock 端到端验收：先前的 HTTP/1 curl 原型已由跨进程 h3x/TCP 后端替换。dhttp 的泛型 Network 在测试构建选择 TcpTransport，以单条回环 TCP 连接承载双向请求流和单向控制/QPACK 流；默认构建仍选择 QuicTransport。Pishoo 的 `Server.listen` 仍只调用 Endpoint.listen。`./pishoo/tests/h3x-tcp-smoke.sh` 分别启动 Pishoo 服务端进程和原生 dhttp Endpoint 客户端进程；客户端确认静态文件、前缀/精确代理、三个实际 WASM Lib、八个并发流、256 KiB Echo、POST Echo、请求 trailers、多值响应 trailers、双向流式 Echo 及提前丢弃响应后继续请求均成功，HTTP 响应版本为 HTTP/3，两个 Echo 数据块分别在下一块输入和上传 EOF 前到达。此验证不覆盖 QUIC、TLS 对端认证、QUIC RESET 语义或路径发现。

Lib API 默认拒绝登记后：Pishoo 50 项库测试通过，新增测试确认首次重载写入 `Deny/All`、管理员改为匿名允许后再次重载不会覆盖。h3x/TCP 集成脚本重新通过，演示环境显式放行的三个 Lib API、静态文件与两种代理均成功。

2026-09-28 简化 Lib 加载与重载后：移除上述自动登记；`load_libs` 直接更新 Sandbox，`api_router` 从当前集合建路由。Pishoo 52 项库测试通过，覆盖加载失败保留旧版本、删除 Lib 或整个 lib 根目录后的入口撤销、重载不写 ACL、未匹配规则的非 owner 拒绝与 owner 允许。

TCP 样例不再写根路径的匿名 Allow，只精确放行测试使用的 GET 静态/代理路径和从 WASM 清单解析出的 Lib 方法与路径；新增跨进程请求确认 `GET` 和带请求体的 `POST /unlisted` 均返回 403。TCP mock 的 StopSending 只释放本端读取方向，避免把双向流的响应方向误当作 request reset；它仍不模拟完整 QUIC STOP_SENDING 错误码传播。

交互示例 `pishoo/examples/echo-interactive.py` 在 TCP 模式自动启动独立服务端与 `pishoo-client`，单个双向流 POST 接收多行输入并逐行回显；QUIC 模式连接已有身份与地址。Echo 组件按 4 KiB 块读取并逐块 flush 响应。

`tcp-mock` 构建的 `pishoo` 主入口与默认构建相同，从现有 `DHTTP_HOME` 加载身份、配置及 Lib。固定演示身份、WASM 和匿名规则改由独立 `setup-tcp-demo` example 创建；演示放行路径与方法从写入后的 `lib.wasm` OpenAPI 清单解析，不在服务入口硬编码。

2026-09-28 本机代理调整后：`proxy_pass` 接受裸回环地址端口或 `http://` 回环 URI，`proxy` 不再接收 dhttp Endpoint；以 Hyper HTTP/1.1 连接本机上游，按目标设置 Host、清理逐跳头，不自动生成 `X-Forwarded-*`。Lib 出站保持 dhttp。57 项 Pishoo 库测试通过，实际本机上游测试覆盖 POST、路径和 Host 改写、响应、多值头，以及上传未结束时逐块返回响应和模拟空闲31秒后的继续传输；h3x/TCP 跨进程 smoke 改由独立本机 HTTP 服务提供代理上游，也验证了双向流式响应在上传 EOF 前到达。

示例目录只保留交互入口和 WASM 编写示例；h3x/TCP 回归脚本与其 Rust 客户端、样例环境初始化移到 `pishoo/tests/`，打包与重建工具移到 `pishoo/tools/`。交互入口复用该客户端，编译期选择默认 QUIC 或 `tcp-mock`，对单个 POST 长连接持续上传和回显。TCP 模式逐行验收确认首行在发送第二行前返回，第二行在上传 EOF 前返回；QUIC 模式已完成编译链接，真实连接依赖外部身份与路径环境，未在本地样例中验收。

首批实现验证结果：Pishoo 47 项、dhttp 27 项、h3x 149 项通过。

Sandbox 拆分后：Pishoo 50 项测试通过，覆盖不同身份的执行槽隔离、重载沿用同一 Sandbox、等待实际任务/permit 回收，以及关闭超时仍保留未完成任务的资源所有权。

WASM 职责集中到 Sandbox 后：Pishoo 56 项测试通过，编译与格式检查通过。新增验证覆盖候选摘要改变时保留旧版本与取消状态、Router 快照共享执行额度、HEAD/OPTIONS 显式声明、API 命名空间隔离，以及通过实际 WASM 执行验证 Sandbox 关闭回收。

取消 Lib 并发限制后：Pishoo 56 项测试通过，覆盖8个同时在途的实际 WASM 请求、关闭后旧 Router 拒绝执行，以及任务回收、内存/fuel 和超时约束。

改用普通 `mod`、库根文件改名及 Sandbox 四文件合并后：Pishoo 56 项测试、工作区全部目标编译检查和 `cargo fmt --all --check` 通过。

扩展到三仓的模块合并后：h3x 162 项测试通过；dhttp 工作区启用 access 的 migration/http 特性后，256 项测试及20项文档测试通过。两仓在临时副本使用 stable 完成全量测试，应用补丁后逐文件确认源码与测试副本相同；Pishoo 再使用原仓库 nightly 工具链运行56项测试，验证实际三仓依赖链。三仓 nightly 格式检查通过。

文件整理后保持上述测试通过，并复验 dhttp 子库：home 31 项、identity 112 项、access 启用 migration/http 时 49 项及 20 项文档测试、log 12 项单元测试及 25 项集成测试。

单命令 exec 替代交互终端、同步当前 Network 生命周期后：Pishoo 51 项单元测试通过。exec 测试覆盖正常输出与退出码、单次主动取消后的直接子进程回收、无限输出触发上限并结束进程、分片内存 Body 请求得到 JSON 结果、关闭任务登记后拒绝待续传 Body、超过四个并发命令，以及 `/exec` 在应用 Router 中挂载。可信远端身份的正向路径尚未在真实跨端请求中验证；主动脱离进程组的后代不在本版保证内。

2026-09-28 全 Pishoo 精简后：移除实例锁、listener JoinSet、整体停机期限及 `Sandbox::verify_libs`；身份或 Lib 加载失败直接返回。运行入口与单身份服务合入 `server.rs`。51 项库测试通过，h3x/TCP 跨进程 smoke 脚本通过；该脚本需允许本机回环端口绑定。

随后按用户明确决定移除 `Invocation.producer_cancel` 与 `LibResponseBody` 的两个 `cancel_on_drop` 字段。Body 丢弃不另行取消 guest；Lib 删除或关闭不再主动取消在途执行。没有后续 I/O 的 guest 可能继续运行，直到自行结束。

随后按用户要求删除 Server.close 的停止监听调用。Pishoo 不保留 listener 句柄，删除身份时关闭应用 Router 与任务，原监听持续到进程退出；同名身份恢复需要重启进程。当前 Pishoo 51 项库测试与 h3x/TCP smoke 脚本通过。

随后按用户要求删除 dhttp 的 `OPERATION_TIMEOUT` 及全部使用点。dhttp 库测试 17 项通过，新增测试覆盖请求 future 和响应 Body 提前丢弃时上传任务的取消；`cargo fmt -p dhttp --check` 与两仓 `git diff --check` 通过。

随后用户批准暂缓 Lib 出站：删除 `HostOutgoing`、`StoreData.outgoing` 和 `Invocation.endpoint`，移除出站子任务与取消链。`StoreData.deny_outgoing` 是 Wasmtime 所需的无状态 hook，始终拒绝 guest HTTP 出站；身份验证仅支持本端与当前握手对端。TaskTracker 直接跟踪 guest，反代本机 HTTP/TCP 保持独立。Pishoo 53 项库测试（含需要本机回环的代理测试）及所有 target 编译检查通过。

随后用户批准删除 `LibResponseBody`：收到 outparam 后直接适配 Wasmtime 原生响应 Body，成功响应不再保存 guest JoinHandle 或等待其结果；TaskTracker 继续跟踪 guest。无响应时仍等待任务以报告错误。删除两项只验证旧包装行为的测试，并将上传错误测试调整为验证原生 Body 结束和任务回收。Pishoo 51 项库测试、所有 target 编译检查、格式及 diff 检查通过；其中两项本机 TCP 代理测试在允许回环绑定的环境下通过。

随后用户批准不改 h3x、仅修正 dhttp 响应 Body 失败路径：转发 Body 返回错误时，先通过现有 `Response<Write>::cancel(H3_REQUEST_CANCELLED)` 取消响应流，再结束并发写入。内存 H3 测试现验证客户端收到部分数据后，继续读取会得到 `H3_REQUEST_CANCELLED`，而不只检查服务端错误。dhttp 的 17 项库测试及 8 项集成测试、格式和 diff 检查通过；未改 h3x 结构或接口。

2026-09-29 用户批准 `/.pishoo/dhttp/{target}` 固定前缀的 DHTTP 正向代理。Pishoo 复用当前 Server 的 Endpoint，要求握手来访者同名且 SKI owner_hash 相同，重写目标 URI 与 Host，清理逐跳头和入站可信 extensions，流式传递请求及响应；配置反代仍只使用回环 HTTP/TCP，Lib WASI HTTP 出站仍拒绝。针对性测试覆盖根路径、query、编码路径、证书序号目标、非法目标及匿名拒绝；Pishoo 55 项库测试通过，其中两项本机 TCP 代理测试在允许回环绑定的环境中运行。真实跨端成功路径仍受下述 qconn 路径发现缺口限制。

随后按用户要求将该入口合并为单条 `/.pishoo/dhttp/{*path}` 路由，支持目标根路径有无末尾斜杠及子路径。路由级测试验证三种形式都进入 DHTTP 处理器，URI 测试验证带末尾斜杠与 query 转发为目标根路径；Pishoo 56 项库测试在允许回环绑定的环境中通过。

按用户要求，统一 DHTTP 通配路由保持纯转发。联系人申请改由 daccess 的 `POST /contact/{name}` 管理路由发起：daccess 调用扩展后的 ContactNotifier trait，Pishoo 用本身份 Endpoint 发送申请并从出站响应的 RemoteAuthority 提取 Bob SubjectId，daccess 随后调用现有 `create_contact` 写入 Alice 本地 Pending 与精确回调规则。Bob 首次 `POST /contact` 仍需其 daccess 策略允许或审批，Pishoo 不绕过授权。Pishoo 库编译检查与 56 项库测试通过；其中 2 项本机 TCP 代理测试需允许回环绑定。daccess 的 68 项库测试使用相同源码和临时清单修正已删除的可选 dhttp-identity 路径后通过，dhttp 的 18 项库测试通过。tcp-mock 不提供已验证对端证书，真实跨端成功路径仍待 QUIC 路径发现完成后验收。

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

流测试不要求真实 dquic 联网，覆盖提前响应、背压、多值 trailers、Body 读写结果和 HEAD/204/304。Pishoo 测试不持有 QPACK 或 H3 writer。

## 尚未完成的验收

- 当前 qconn 出站连接仍缺少实际路径发现；真实跨端请求及联系人通知的成功路径尚不能据此宣称完成。ContactNotifier 已接入现有 Endpoint；无法建立对端连接时由 daccess 保留 Syncing 供重试。这里保留其既有接口，以内存流验证上层通信行为。
- exec 使用服务账号权限，不提供 OS 沙箱。主动脱离本次进程组的后代不在本版回收保证内；不宣称支持交互终端或任意恶意命令的完整资源隔离。
- Workspace 当前提供列表查看和审批操作；完整联系人/规则编辑交互仍待完善。管理 API 已直接使用当前 daccess 库。

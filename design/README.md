# 三仓第一版设计清单

日期：2026-09-26。状态：按用户确认持续维护的冻结接口基线；实现及验收进度见[实施记录](../IMPLEMENTATION.md)。

## 设计准则

**没有当前用途的结构、成员和函数不增加。** 每个新增项必须能回答：它支持哪一个已经要求的操作，复用现有类型为什么不够。

- 优先持有实际资源，避免通用 State/Context/Manager 包装；真正有状态转换时才定义表达该转换的 enum。
- 可从现有对象取得的信息不再保存，固定值不先做成用户配置。
- bool、计数器、取消/通知信号必须对应不可省略的当前约束；不能为了未来监控、精细报告或假想并发流程提前加入。
- 状态变化优先通过正在执行的函数返回值表达。流的 EOF/错误由 read/write 返回，不新增 error.await、terminate.await 旁路。
- 互斥状态用 enum 或 Result<正常对象, Error> 表达，只在对应分支持有有效资源；不并列保存正常对象、error、failed/closed 等可能矛盾的标志。
- 同一结构的成员应表达可独立成立的资源或事实；不要用多个相关 Option/bool 组合出若干非法状态。仅在特定阶段有效的成员移入该阶段的 enum 分支。
- 不新增 XxxGuard 类型；必要的局部释放和取消复用已有所有权与库工具。
- 不冻结纯局部 helper 的实现拆分，更不把 helper 包装成一层公共 API。
- 不用重命名、匿名元组、扩展表或额外全局变量藏回已删除的状态。

## 唯一入口

以下文件共同构成当前设计。类型、字段与方法以三个结构清单为准；架构说明解释职责和行为，不独立增加接口。

| 文件 | 内容 |
| --- | --- |
| [三仓架构](h3x-dhttp-pishoo-architecture.md) | 仓库职责、数据路径、应用约束和实施次序 |
| [dhttp 结构](dhttp-interfaces.md) | 独立 Endpoint、全局 Network、连接复用与应用接入 |
| [Pishoo 结构](pishoo-interfaces.md) | 简单配置、Server/Router/Sandbox/Lib、daccess 接入与 WASM |
| [exec 结构](exec-interfaces.md) | 单命令宿主执行、身份准入与子进程回收 |

## 冻结规则

1. 冻结范围覆盖 HTTP 网关、WASM 及单命令 exec。每个自定义有状态结构的私有成员也在范围内。
2. 清单列出的结构名、字段名及类型、枚举变体及载荷、方法签名、跨模块函数签名和调用归属，后续实现不得自行增加、删除或修改。
3. 方法体、局部变量、闭包及编译器生成的 async 状态可以按实现需要编写。模块内部的无状态辅助函数可拆分算法；它们不能新增跨模块接口或持久状态。
4. 不允许用 `Any`、通用属性包、未限定的 extensions、占位成员或匿名集合隐藏清单之外的状态。清单中的集合只能存其明确列出的业务内容。
5. 已复用的第三方类型按所选依赖版本使用，不复制新模型。后续依赖升级若改变冻结接口，按接口变更处理。
6. 发现清单无法满足实现时，先列出具体冲突、受影响调用和最小变更，取得用户明确同意后再修改清单及代码。不得在“顺手重构”中扩展结构。
7. 没有在结构清单中列出的能力不通过预留字段进入代码。exec 的 OS 权限和后代回收限制必须如实报告，不能用文档替代实现验收。
8. 这份基线冻结的是设计，不代表实现已经编译、联网或通过隔离测试。结构实现与行为验收分别检查。

## 当前边界

- h3x 不新增或修改结构、字段、接口。dhttp 使用已确定的 `open_bi`、`accept_bi`、`read_request`、`read_response`、`write_request`、`write_response`。
- Endpoint 独立 `load(name)`；Network 全局初始化。Endpoint 不持 Network、QUIC endpoint 或连接。
- 同规范化名称代表同一逻辑 Endpoint；多次 load 通过全局 Network 的同一身份连接池复用连接。Endpoint 不提供 close 或 stop_listening。Network 属于进程生命周期，不提供 shutdown。
- 当前不设计全局或逐 Endpoint 的网络传输配额，不预留配额字段、permit 或租约结构；保留流级背压、连接超时和单次执行限制。Lib 与 exec 都不设并发名额。
- 配置反代只允许本机 HTTP/TCP 上游；另设同名身份专用的 `/.pishoo/dhttp/{target}` 前缀，用当前 Server 的 Endpoint 正向代理 DHTTP 请求。Lib 出站暂不实现，WASI HTTP 出站请求一律拒绝。不增加 UpstreamKind、传输选择字段或连接失败后的回退。
- 2026-09-27 用户批准 `tcp-mock` 构建例外及泛型 Network：默认后端仍用 QUIC；测试后端把 h3x 的双向请求流和单向控制/QPACK 流复用在回环 TCP 上。测试客户端是独立进程中的 dhttp Endpoint，不经过 HTTP/1 桥；Pishoo 在测试连接上注入匿名远端摘要。已批准的 Network/后端成员变更见 dhttp 清单；h3x 接口不变，此验证不代表 QUIC、TLS 对端认证或路径发现通过。
- dhttp 保留 `endpoint.get(url).header(...).await`；URL/header 使用已校验类型，解析错误立即返回。Lib 暂不调用该出站接口。
- 不新增 dhttp Body 结构；复用 h3x 原生流，标准 Service 接缝仅用现成 StreamBody/UnsyncBoxBody 适配。
- 完成和取消使用流式 EOF、错误、stop、cancel 及读写 future 的结果。没有 ExchangeControl 或公开 finished。
- 身份直接复用 qtls 的 HandshakeSummary、LocalAuthority、RemoteAuthority，范围复用 qconn 的 Scope/Scopes。没有 RequestInfo 或 Peer 包装。
- 2026-09-28 用户确认入站 URI authority 简写展开及与握手本端身份的核对归 dhttp 的 `serve_exchange`，在交付应用 Service 前完成；Pishoo 的 `Server.listen` 不重复执行。缺少或不匹配的 authority 由 dhttp 返回 421。
- 一个 WASM 文件统一称为 Lib，不另设 App；代码类型使用 Lib 和通用 Body/Error。
- 每个 Server 直接持有一个 Sandbox，集中拥有该身份的 Lib 集合、共享 WasmRuntime 引用与任务跟踪器；组件扫描、校验、版本替换、API 执行和 WASI 宿主能力均归 sandbox 模块。Lib 执行不限制并发数，不设置执行槽或 permit；Sandbox 不新增内部锁、取消信号、派生计数或策略容器，实际隔离由 Store、WasiCtx、limiter/fuel 和宿主能力实现。
- WASM 执行归 Pishoo；h3x 和 dhttp 不依赖 Pishoo 或 Wasmtime。
- WASM 不设总执行时长期限；每次调用仍受 Store 中逐 linear memory 的内存限制、fuel 和 WASI 宿主能力约束，guest 任务由 TaskTracker 跟踪。
- daccess 的当前库接口是授权、审批和管理路由的依据；尽量复用 `pishoo/feat/daccess` 的集成，不兼容处按库调整。审批在当前请求中等待库返回的结果，不新增审批状态结构或后台等待任务。联系人通知由 Pishoo 使用现有 Endpoint 实现 daccess 的 ContactNotifier。Lib API 不自动登记访问规则，不建立导入账本。
- 不增加 Server 级统一请求并发限额或应用租约；静态/代理直接使用现成 Body，Lib 和 exec 各自管理实际执行资源。
- 第一版串行加载/重载，Server 直接持有 Router 和 Sandbox，Sandbox 直接持有 Lib；Sandbox 扫描时使用局部候选集合，校验成功后更新自身 Lib，再由 Server 构建并替换 Router。不建立 ServerState、Release 或 begin_build 发布流程。
- Pishoo 启动时加载身份与配置，运行中仅在收到 SIGHUP 时扫描并串行重载；不定时轮询。`listen` 和 `exec` 变化仍需重启。

2026-09-26 用户确认将 WASM 职责集中到 Sandbox：在已有 `lib_slots`、`tasks` 基础上迁入 Server 的 `libs`、`runtime`，Server 改为直接持有 `Sandbox`；组件加载、版本替换和 API Router 构造方法归 Sandbox。当时 `build_router` 接收已构造的 Lib Router；其后改为由 Server 显式组装完整 Router。运行入口保留跨身份共享的 WasmRuntime，Server 保留 Endpoint、授权和整体 Router 发布。当次迁移保持 Invocation 与 Store 的成员及调用签名，`validate_lib` 的根级公开导出不变。字段和方法的完整签名见 [Pishoo 清单](pishoo-interfaces.md)。

2026-09-27 用户要求移除 DaemonConfig 与 Daemon 结构：`run` 以局部变量持有实例目录、Server 和 WasmRuntime，保留串行重载及关闭顺序。随后用户取消实例配置文件和 TerminalPolicy，并将第一版交互终端收缩为单命令宿主 exec：每个 Server 的 schema v1 settings 单行包含执行开关与同名身份准入，删除终端会话、WASM shell 和平台隔离后端，Server 直接持有 exec 任务跟踪器。执行开关现命名为 `exec`，接口以 [Pishoo 清单](pishoo-interfaces.md) 和 [exec 清单](exec-interfaces.md) 为准。

2026-09-28 用户要求移除 `exec_slots` 并发名额与 `Server.cancel` 身份取消信号。exec 与 Lib 不设并发名额；Server 关闭时清空当前 Router、关闭任务跟踪器，Lib 在途执行自然完成，exec 已启动命令仍由单次请求取消或超时机制负责回收。

2026-09-28 用户进一步要求删除 Lib 路由、Invocation 构造和执行入口反复检查 `Lib.cancel`、`Sandbox.tasks.is_closed()` 的提前拒绝。旧 Router 若仍被请求持有，关闭后的 TaskTracker 仍可登记新任务，关闭等待只覆盖当时及等待期间跟踪到的任务。

2026-09-27 用户确认同步当前 dhttp 无参数 Network 初始化：移除 NetworkConfig、ListenConfig 和 Pishoo 的 `network_config` 接缝，`DhttpNetwork::init()` 无参数；各 Server 在 `Endpoint.listen` 时交付自己的范围。完整接口以 [dhttp 清单](dhttp-interfaces.md) 为准。

2026-09-27 用户确认移除 Network.shutdown：Pishoo 退出先停止各 Server 监听，并等待自己的应用任务；全局 Network 与连接池保持到进程退出，不再承诺运行中主动关闭全部传输。接口以 [dhttp 清单](dhttp-interfaces.md) 为准。

2026-09-26 用户要求取消 Lib 并发限制：删除 `Sandbox.lib_slots`、`Invocation.permit`、`StoreData.permit`、`Invocation::new` 的 permit 参数和 `Error::Capacity`；保留单次执行的内存、fuel 及超时限制。完整签名与行为见 [Pishoo 清单](pishoo-interfaces.md)。

2026-09-27 用户要求取消 WASM 固定 30 秒总执行期限：长时间流式或等待 I/O 的调用可以继续运行，单次内存、fuel 与出站 I/O 超时仍保留；不增加 Lib 并发名额或新的状态字段。

2026-09-28 用户要求删除 `LibPolicy` 与 `OutgoingRule`。相应删除 `Lib`、`StoreData`、`HostOutgoing` 的 policy 成员及 `Lib::load` 的 policy 参数。`/data` 固定可写；签名与验证能力开放；可信同名调用者的 Lib 出站使用当前身份 Endpoint，并保留 URI 与管理路径校验。不新增替代策略状态。

2026-09-28 用户要求删除 `HostOutgoing.remaining_requests` 及固定 16 次出站上限；HTTP 出站和远端签名验证解析仍逐次检查目标授权，并保留取消和 I/O 超时。

2026-09-28 用户要求将每 2 秒扫描改为显式触发：运行中的身份、配置和 Lib 重载由 SIGHUP 发起，不增加状态容器或后台轮询任务。

2026-09-28 用户将 ServerConfig 的执行开关及 config.db 的对应列统一命名为 `exec`。schema 仍为 v1，直接修改 v1 建表定义与读取逻辑，不增加迁移版本。

2026-09-28 用户明确要求移除冻结成员 `Lib.cancel`，不再处理同名 Lib 新旧版本共用取消 token 的场景；`Lib::load` 同步删除 cancel 参数。Lib 删除与 Sandbox.close 仅撤销路由、清空集合并等待已登记任务，不主动取消在途 guest。`HostOutgoing.cancel` 仍是每次调用的局部出站收尾信号，不属于 Lib 状态。未自行结束的 guest 可能使关闭等待达到15秒上限。

2026-09-28 用户要求精简 Pishoo：移除进程实例锁、listener JoinSet 和运行入口的整体停机期限；`daemon` 模块并入 `server.rs`。用户明确批准删除冻结接口 `Sandbox::verify_libs`，同时取消单身份或单 Lib 加载失败时的跳过及旧版本回退；加载失败直接结束启动或本次重载。身份校验、daccess 授权、输入校验、WASM/exec 限额和单次超时保留。

2026-09-28 用户明确要求 WASM 响应的传输断开通过 Body 读写结果体现，不保留独立的丢弃取消句柄：删除冻结成员 `Invocation.producer_cancel` 与 `LibResponseBody::Reading/Waiting.cancel_on_drop`。Lib 删除和关闭不主动取消在途执行；Body 提前丢弃后没有继续 I/O 的 guest 可能运行到自行结束，关闭等待超过15秒则返回超时。

2026-09-28 用户进一步批准删除冻结类型 `LibResponseBody` 及其全部变体。收到 outparam 后直接适配 Wasmtime 原生响应 Body；guest 仍由 TaskTracker 跟踪，但响应 Body 不再持有 JoinHandle、等待 guest 结果或将其后续失败转换为 Body 错误。未收到响应时仍读取任务结果以报告启动或执行失败。

2026-09-28 用户要求先删除停止监听接口。Pishoo 的 Server.close 不再调用 Endpoint.stop_listening；run 不保留 listener 句柄，故删除身份时只能关闭应用 Router 和任务，监听登记持续到进程退出，同名身份恢复需要重启进程。

2026-09-28 用户要求简化 Lib 加载与重载：`Sandbox::load_libs(&mut self, profile) -> Result<()>` 在完整扫描成功后直接更新 `Sandbox.libs`，删除 `replace_libs`；`api_router(&self, endpoint)` 直接读取现有 Lib。启动和重载均按加载 Lib、构建 Router 的顺序执行。Lib API 不再自动写入 daccess 默认拒绝规则；未匹配规则时由 daccess 的默认策略处理。

2026-09-28 用户要求将静态文件与代理分开：以普通 `/file/{*path}` 静态 Router 和代理 fallback 取代冻结的 `proxy_or_static` 跨模块函数，分别使用 `file_router` 与 `proxy_pass`。`/file/a` 映射身份目录 `file/a`，`/file` 本身不支持；代理按路径段前缀匹配，`/foo` 不再匹配 `/foobar`，未命中配置时返回 404。

2026-09-28 用户明确要求反代本机 TCP HTTP 服务，取消反代到 dhttp 名称。`proxy_pass` 接受 `127.0.0.1:8080` 或 `http://127.0.0.1:8080[/路径]` 等回环地址；`ProxyLocation.proxy_pass` 字段保持 `http::uri::Parts`，冻结函数 `proxy` 和 `proxy_pass` 删除不再使用的 Endpoint 参数。Lib 出站继续只走 dhttp。代理以普通 HTTP/1.1 客户端连接本机上游，清理逐跳头但不自动生成 `X-Forwarded-*`。

2026-09-28 用户要求删除 dhttp 的 `OPERATION_TIMEOUT` 及全部单次开流、消息头和 Body 读写超时。dhttp 继续以流 EOF、读写错误和取消处理生命周期；上层可按需要取消请求 future 或丢弃 Body。Network 的 `CONNECT_TIMEOUT` 与 Pishoo 自己的业务期限不受影响。

2026-09-26 用户要求改用普通 `mod` 和同名 `.rs` 文件，并合并过碎的 Sandbox 文件。Pishoo 工作区库入口使用 `pishoo.rs`、`gateway.rs`，由 Cargo `[lib].path` 指定；测试同样使用普通模块。该文件组织调整允许现有成员和无状态函数在原职责范围内使用必要的 `pub(super)` 及显式导入；类型字段、函数参数和运行行为保持不变。Sandbox 的四文件划分见 [Pishoo 清单](pishoo-interfaces.md)。

2026-09-26 用户将同一整理要求扩展到 h3x 和 dhttp：合并同一类型或同一职责的实现片段，手写实现使用普通模块，库入口使用 crate 同名文件。h3x 的帧载荷、QPACK 编码和流读取实现收拢；dhttp 的 Endpoint、名称、身份、SSL 和访问策略实现按职责收拢。公开导出、结构字段、枚举载荷和方法签名保持不变；模块内部按现有调用关系调整导入和必要的父模块可见性。

2026-09-28 用户批准暂缓 Lib 出站并删除相关状态：移除 `StoreData.outgoing`、`HostOutgoing` 和 `Invocation.endpoint`，以 `StoreData.deny_outgoing: DenyOutgoing` 的无状态 WASI hook 明确拒绝所有 guest HTTP 出站。`Invocation::new` 仍接收 Endpoint 以核对握手本端身份。guest 直接由 TaskTracker 跟踪，不再创建出站子任务或取消信号；签名验证仅使用本端或当前握手对端的公钥。反代本机 HTTP/TCP 不受此变更影响。

2026-09-28 用户要求 OpenAPI JSON 以 oas3 反序列化成功为准，删除额外的重复键与外部引用扫描；路径级引用因无法提供路由方法仍拒绝。`validate_lib` 签名及其他路径、版本和组件校验不变。

2026-09-29 用户批准在 Pishoo 实现 daccess 既有的 `ContactNotifier`，由本 Server 的 Endpoint 直接发送联系人授权更新，不修改 daccess。冻结新增 `DhttpContactNotifier { endpoint }` 及其 trait 方法，并为管理路由装配函数增加 Endpoint 参数；Server 与 dhttp 均不新增成员。通知失败由 daccess 保留 Syncing 供重试。

2026-09-29 用户批准新增同名身份专用的 DHTTP 正向代理：`/.pishoo/dhttp/{target}` 及其子路径由 `routes::forward_dhttp(endpoint, request) -> Response` 处理，复用 Server 已有 Endpoint，不增加成员或替换本机 HTTP/TCP 代理。请求先经过 daccess 授权，再核对握手来访者与本 Server 同名且 SKI owner_hash 相同；目标仅为 DHTTP 名称。Lib 的 WASI HTTP 出站仍拒绝。

## 文档清理

本仓此前的接入稿、Pishoo API 草案、数据库重设计、WASM HTTP 适配、身份沙盒、终端旧稿及旧架构图由本组文档替代。仍有效的应用规则已归入架构说明和结构清单。

README 的安装说明、CHANGELOG 的历史记录和 CONTEXT 词汇表不承担接口定义。其他仓库中的历史设计也不作为本轮三仓接口的实现依据。

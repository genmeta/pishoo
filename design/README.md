# 三仓第一版设计清单

2026-10-09 用户批准为 OpenCode Web 允许显式 `/file` 代理覆盖匹配的静态文件路径；精确和路径段前缀规则均可使用，根代理不覆盖静态文件，未匹配的静态路径保持原行为。只改现有配置校验和 Router 方法体，不新增结构、字段或函数签名。部署于 code.alice.smith，并授权 alice.smith。

2026-10-09 用户批准在主目录修复 h3x 的 QPACK 反馈：Decoder::State 用有界 feedback 队列替换 on_instruction，新增 reported_insert_count；未提交插入计数由现有动态表计数减它推导。Decoder 增加 take_feedback；对应 State 算法、私有 write_decoder/sync_decoder_with 与连接装配改为直接读取 Decoder 反馈，复用 ArcQpack 已有 watch 唤醒。ACK 无空间时保持解码 Pending，同步取消使用保留空间；反馈空间和等待字段字节保持有界。删除不再使用的 Decoder 回调方法及接收队列别名，公开接口、其余结构及 h3x 职责不变；不新增 Guard、传输配额或工作树。

2026-10-08 用户批准代理与 Lib 的 `/api` 路由兼容策略：取消全局 `/api` 保留，只注册已加载 `/api/<LibId>` 的根及子路径；该前缀内404/405不回退代理，其他 `/api/*` 走配置代理。Server.load 拒绝落入已加载 Lib 前缀的显式代理 location，并列出 location/LibId；`/` 或 `/api` 等更宽代理可作兜底。管理命名空间仍保留。不新增结构、字段或方法/跨模块函数签名。

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
- 自定义函数按具体动作命名，不使用 `sync_` 前缀；区分维护资源、刷新远端结果和应用更新。
- 不冻结纯局部 helper 的实现拆分，更不把 helper 包装成一层公共 API。
- 不用重命名、匿名元组、扩展表或额外全局变量藏回已删除的状态。

## 唯一入口

以下文件共同构成当前设计。类型、字段与方法以结构清单为准；架构说明解释职责和行为，不独立增加接口。

| 文件 | 内容 |
| --- | --- |
| [三仓架构](h3x-dhttp-pishoo-architecture.md) | 仓库职责、数据路径、应用约束和实施次序 |
| [dhttp 结构](dhttp-interfaces.md) | 独立 Endpoint、全局 Network、连接复用与应用接入 |
| [管理命令与 API](../pishoo/docs/management-design.md) | 已批准管理行为、命令、磁盘原子操作和验收 |
| [Pishoo 结构](pishoo-interfaces.md) | 简单配置、Server/Router/Sandbox/Lib、daccess 接入与 WASM |
| [Workspace/Chat 接入](workspace-chat-interfaces.md) | 目标分支业务模型、Server 资源归属及 Endpoint 出站接缝 |
| [DNS 解析与发布](pishoo-dns-detailed-design.md) | 全进程解析源、内存凭据发布、地址维护、租期和自然过期 |

## 冻结规则

1. 冻结范围覆盖 HTTP 网关和 WASM。每个自定义有状态结构的私有成员也在范围内。
2. 清单列出的结构名、字段名及类型、枚举变体及载荷、方法签名、跨模块函数签名和调用归属，后续实现不得自行增加、删除或修改。
3. 方法体、局部变量、闭包及编译器生成的 async 状态可以按实现需要编写。模块内部的无状态辅助函数可拆分算法；它们不能新增跨模块接口或持久状态。
4. 不允许用 `Any`、通用属性包、未限定的 extensions、占位成员或匿名集合隐藏清单之外的状态。清单中的集合只能存其明确列出的业务内容。
5. 已复用的第三方类型按所选依赖版本使用，不复制新模型。后续依赖升级若改变冻结接口，按接口变更处理。
6. 发现清单无法满足实现时，先列出具体冲突、受影响调用和最小变更，取得用户明确同意后再修改清单及代码。不得在“顺手重构”中扩展结构。
7. 没有在结构清单中列出的能力不通过预留字段进入代码。不提供宿主命令 exec 入口。
8. 这份基线冻结的是设计，不代表实现已经编译、联网或通过隔离测试。结构实现与行为验收分别检查。

## 当前边界

- h3x 不新增或修改结构、字段、接口。dhttp 使用已确定的 `open_bi`、`accept_bi`、`read_request`、`read_response`、`write_request`、`write_response`。
- Endpoint 独立 `load(name)`；Network 全局初始化。Endpoint 持已加载的 `Arc<qconn::QuicEndpoint>`，不持 Network 或连接；DNS 签名与 TLS 共用同一组内存凭据。
- 同规范化名称代表同一逻辑 Endpoint；多次 load 通过全局 Network 的同一身份连接池复用连接。Endpoint 不提供 close 或 stop_listening。Network 属于进程生命周期，不提供 shutdown。
- 当前不设计全局或逐 Endpoint 的网络传输配额，不预留配额字段、permit 或租约结构；保留流级背压、连接超时和单次执行限制。Lib 不设并发名额。
- 配置反代只允许本机 HTTP/TCP 上游；另设同名身份专用的 `/.pishoo/dhttp/{*path}` 路由，用当前 Server 的 Endpoint 正向代理 DHTTP 请求。Lib 出站暂不实现，WASI HTTP 出站请求一律拒绝。不增加 UpstreamKind、传输选择字段或连接失败后的回退。
- 2026-10-02 按用户指定的 [Network 详细设计](../../dhttp/docs/design/network-detailed-design.md)统一为 QUIC。Network 只保存服务表和 H3 连接池；幂等 init 准备全部可用网卡，由 netwatcher 事件触发扫描，socket/地址登记及清理交给 dquic Dock。listener scopes 只限制名称来源；取消 listener 保留 socket。接入回调直接装配 H3，匿名出站由 Request::new 创建。池键用 Incoming、Outgoing 分别约束本端或远端身份必填；双方具名时按名称对跨方向复用，qconn 的本端 identity 可选。原泛型后端、TCP mock、BackendState、Binding 和 ListenerEntry 从当前清单移除。
- dhttp 保留 `endpoint.get(url).header(...).await`；URL/header 使用已校验类型，解析错误立即返回。Lib 暂不调用该出站接口。
- 不新增 dhttp Body 结构；复用 h3x 原生流，标准 Service 接缝仅用现成 StreamBody/UnsyncBoxBody 适配。
- 完成和取消使用流式 EOF、错误、stop、cancel 及读写 future 的结果。没有 ExchangeControl 或公开 finished。
- 身份直接复用 qtls 的 HandshakeSummary、LocalAuthority、RemoteAuthority，范围复用 qconn 的 Scope/Scopes。没有 RequestInfo 或 Peer 包装。
- dhttp 出站响应在进程内的 extensions 携带实际连接已验证的 RemoteAuthority；不恢复已删除的 resolve_remote。Pishoo 的统一 DHTTP 通配路由纯转发。联系人申请以目标分支的 application_id、Workspace 队列和 /contact/self 轮询协议为准；2026-10-03 用户要求接入 Workspace/Chat 生产出站，并批准 dhttp Request 的发送前 owner_hash 校验，具体成员见 dhttp 清单。
- 2026-09-28 用户确认入站 URI authority 简写展开及与握手本端身份的核对归 dhttp 的 `serve_exchange`，在交付应用 Service 前完成；Pishoo 的 `Server.listen` 不重复执行。缺少或不匹配的 authority 由 dhttp 返回 421。
- 一个 WASM 文件统一称为 Lib，不另设 App；代码类型使用 Lib 和通用 Body/Error。
- 每个 Server 直接持有一个 Sandbox，集中拥有该身份的 Lib 集合、共享 WasmRuntime 引用与任务跟踪器；组件扫描、校验、版本替换、API 执行和 WASI 宿主能力均归 sandbox 模块。Lib 执行不限制并发数，不设置执行槽或 permit；Sandbox 不新增内部锁、取消信号、派生计数或策略容器，实际隔离由 Store、WasiCtx、limiter/fuel 和宿主能力实现。
- WASM 执行归 Pishoo；h3x 和 dhttp 不依赖 Pishoo 或 Wasmtime。
- WASM 不设总执行时长期限；每次调用仍受 Store 中逐 linear memory 的内存限制、fuel 和 WASI 宿主能力约束，guest 任务由 TaskTracker 跟踪。
- daccess 的当前库接口是授权、审批和管理路由的依据；尽量复用 `pishoo/feat/daccess` 的集成，不兼容处按库调整。审批立即返回202，由 daccess 持久保存并提供按 Visitor 校验的状态查询；联系人使用申请队列与轮询，删除 ContactNotifier 回调。Lib API 不自动登记访问规则，不建立导入账本。
- 不增加 Server 级统一请求并发限额或应用租约；静态/代理直接使用现成 Body，Lib 管理实际执行资源。
- 第一版仅在启动时串行加载，身份、配置、Lib 和证书/私钥更新统一重启；OCSP 每72小时更新。Server 直接持有 Router 和 Sandbox，Sandbox 直接持有 Lib；Sandbox 扫描时使用局部候选集合，校验成功后更新自身 Lib，再由 Server 构建 Router。不建立 ServerState、Release 或 begin_build 发布流程。
- Pishoo 启动时加载身份、配置和 Lib；运行中维护 DNS 发布、地址变化，并每72小时刷新已加载身份的 OCSP；不支持 SIGHUP 重载。身份、配置、Lib 与证书/私钥变更需重启。

2026-09-26 用户确认将 WASM 职责集中到 Sandbox：在已有 `lib_slots`、`tasks` 基础上迁入 Server 的 `libs`、`runtime`，Server 改为直接持有 `Sandbox`；组件加载、版本替换和 API Router 构造方法归 Sandbox。当时 `build_router` 接收已构造的 Lib Router；其后改为由 Server 显式组装完整 Router。运行入口保留跨身份共享的 WasmRuntime，Server 保留 Endpoint、授权和整体 Router 发布。当次迁移保持 Invocation 与 Store 的成员及调用签名，`validate_lib` 的根级公开导出不变。字段和方法的完整签名见 [Pishoo 清单](pishoo-interfaces.md)。

2026-09-27 用户要求移除 DaemonConfig 与 Daemon 结构：`run` 以局部变量持有实例目录、Server 和 WasmRuntime，保留串行重载及关闭顺序。随后用户取消实例配置文件和 TerminalPolicy，并将第一版交互终端收缩为单命令宿主 exec：每个 Server 的 schema v1 settings 单行包含执行开关与同名身份准入，删除终端会话、WASM shell 和平台隔离后端，Server 直接持有 exec 任务跟踪器。执行开关现命名为 `exec`，接口以 [Pishoo 清单](pishoo-interfaces.md) 为准。

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

2026-09-29 用户批准新增同名身份专用的 DHTTP 正向代理：`/.pishoo/dhttp/` 前缀由 `routes::forward_dhttp(endpoint, request) -> Response` 处理，复用 Server 已有 Endpoint，不增加成员或替换本机 HTTP/TCP 代理。请求先经过 daccess 授权，再核对握手来访者与本 Server 同名且 SKI owner_hash 相同；目标仅为 DHTTP 名称。Lib 的 WASI HTTP 出站仍拒绝。随后用户要求用单条 `/.pishoo/dhttp/{*path}` 路由覆盖带或不带末尾斜杠的目标根路径及其子路径。

2026-09-29 用户批准联系人申请由 daccess 管理 API 发起：在现有 `/contact/{name}` 增加 POST，并给 `ContactNotifier` 增加 `submit_application(contact, body) -> Future<Result<SubjectId, NotifyError>>`。Pishoo 的 `DhttpContactNotifier` 使用已有 Endpoint 发送到 Bob，核对响应扩展中的已验证对端身份并返回 SubjectId；daccess 收到成功结果后调用现有 `create_contact`。统一 DHTTP outgoing 仍纯转发，不新增 Server 字段或运行时 handler 注册状态。

2026-10-02 用户确认审批和联系人以 `daccess/feat/fit-pishoo@cf8f72f` 为准，随后要求先 rebase 适配，暂缓尚未稳定的底层接口。本地重构已重放至 `pishoo/feat/daccess@9b733c5`，Workspace/Chat 功能按该分支保留并接入 Server。此前的请求内审批、禁止202/status和 ContactNotifier 回调约定由这一决定替代；历史决策段落仅保留其演变记录。生产出站接缝、tcp-mock 与现行底层的兼容及真实网络验收留待后续。具体业务接口见 Pishoo 和 Workspace/Chat 清单。

2026-10-03 用户要求开始 Workspace/Chat 出站接入，并明确批准最小 dhttp 请求成员变更：Request.expected_remote_owner_hash、Request::expect_remote_owner_hash 和 Error::RemoteIdentityChanged。Workspace/Chat 的既有 OutboundTransport 直接由 Endpoint 实现，Server.load 装配现有 Endpoint；Chat 在实际连接开流和发送前检查联系人 owner_hash。该决定替代2026-10-02暂缓生产出站的安排，Pishoo 不新增生产有状态结构。签名及行为见 dhttp 与 Workspace/Chat 清单，验收见实施记录。

2026-10-03 用户要求先取消客户端必须携带 OCSP 的限制，以接入现有 AnySee。qtls ClientVerifier 在证书链与有效期验证后，仅对非空客户端 staple 执行 OCSP 的证书绑定、签名、时效和撤销验证；未附 OCSP 的具名客户端仍以经验证证书及握手签名认证。仍请求支持该能力的客户端提供 staple；服务端证书的 OCSP 要求不变。不新增类型、成员、配置开关，也不更改 HTTP/3 帧格式。

## 文档清理

2026-10-08 用户要求删除 `Lib.digest` 及摘要复用分支。每次启动或 SIGHUP 扫描都重新校验、编译全部 Lib；完整扫描成功后替换集合，失败仍保留当前 Lib 与 Router。具体字段及行为见 [Pishoo 清单](pishoo-interfaces.md)。

2026-10-04 用户要求先兼容官方服务未携带 OCSP，随后明确扩大到所有域名：qtls ServerVerifier 在证书链、域名、有效期验证后允许缺失服务端 staple；已提供的 OCSP 仍校验证书绑定、签名、时效和撤销状态。握手签名校验不变。仅调整方法体，不增加结构、字段、接口或开关；替代此前服务端必须携带 OCSP 的规则。已有 DDNS 显式文件补充路径保留，指定文件仍必须验证；正常启动不再依赖该文件。本地身份 OCSP 加载与续期规则保持原样。

2026-10-04 用户要求身份验证失败跳过该身份，替代2026-09-28身份加载失败直接结束的约定：启动和 SIGHUP 新身份加载记录凭据失败的身份名及原因，继续后续身份；跳过身份下次 SIGHUP 重试。已加载身份在 SIGHUP 凭据读取失败时跳过本次重载并保留内存资源。配置、数据库、Lib 及全局网络错误仍直接返回。不增加结构、字段、错误变体或跨模块函数，具体行为见 Pishoo 清单。

本仓此前的接入稿、Pishoo API 草案、数据库重设计、WASM HTTP 适配、身份沙盒、终端旧稿及旧架构图由本组文档替代。仍有效的应用规则已归入架构说明和结构清单。

README 的安装说明、CHANGELOG 的历史记录和 CONTEXT 词汇表不承担接口定义。其他仓库中的历史设计也不作为本轮三仓接口的实现依据。

2026-10-03 用户明确批准 [DNS 详细设计](pishoo-dns-detailed-design.md) 的全部六项接口及相邻仓库变化：Server.publisher 与六个 dns 函数；确认现行 Endpoint.quic 并新增 local_authority/ListenFuture、修改监听登记返回值；AddressBook.inner_bindings；H3Resolver 直接持 Endpoint、发布返回 Duration 及错误变体调整；服务端租期头/no-store；同步本冻结清单。run 在 Network 初始化前注册 System/H3/mDNS 解析源并订阅地址簿；监听登记成功后维护发布批，SIGHUP、删除和退出先排空当前发布，再清理自己的 mDNS/应用资源；2026-10-08 用户取消主动撤回，停止续期后由记录自然过期。新旧服务端协议部署与公网/NAT 验收仍需分别确认，不改变共享传输的进程生命周期。

2026-10-03 用户要求继续线上端到端验收并包含 NAT 探测与打洞，明确批准临时替换并恢复 code 身份的线上 DNS，随后要求先跳过线上尚未部署的租期头校验。旧发布响应缺头时暂按300秒续期窗口、空发布按0处理；已有租期头仍按原规则校验，签名和身份鉴权不变。qtls 的方法体允许仅为 ddns.genmeta.net 从 DQUIC_DDNS_OCSP_FILE 补充客户端预取的 OCSP，仍执行证书绑定、签名、时效和撤销验证。具名 qconn 出站从同一 LocalAuthority 填充已存在的 ClientName 传输参数，以兼容线上旧版身份识别；不新增类型、字段或方法。NAT 映射登记和仅公网地址的验收装配暂在独立端到端 example 中完成；单元测试不执行线上请求，普通 Network 启动尚不自动探测。

2026-10-03 用户批准配置 API 第一版，按系统设置与代理规则拆为 `GET/PATCH /sys/settings`、`GET/PUT /sys/proxies`，复用 H3、daccess 和 config.db schema v1。仅新增 `setup::config_router(profile, endpoint)` 跨模块函数，现有保留路径检查加入 /sys；不增加结构、字段、数据库表或传输接口。具体签名及权限、生效规则见 Pishoo 清单。

2026-10-03 用户要求普通启动自动进行 NAT 探测，并批准私有 Binding 新增 nat_probe 流成员，随后明确 NAT 分类是每个新 socket 的一次性操作，STUN 绑定心跳则持续维护。Network 初始扫描装配探测，唯一维护任务轮询分类和心跳，映射登记到已有 QUIC/AddressBook；绑定撤回直接丢弃流取消 transaction。该决定替代普通启动尚不自动探测的阶段性边界，完整成员见 dhttp 清单及相邻 Network 详细设计。

2026-10-04 用户指出中转 DNS 应发布 outer-agent。Network 的 QUIC 别名与 DDNS 上报分开：FullCone 映射可发布 Direct，受限或尚未成功分类的映射保留 Mediate(agent, outer)，交既有 E-record 编码输出 outer-agent。AddressBook 的现有外部地址表允许有效 Mediate，内部地址表仍只存 Direct；不新增结构、字段、方法或错误变体。

2026-10-04 用户确认使用 home 身份与根级 ssl/db/file/lib/logs/repo/templates 布局，不再使用 server.conf，并要求实施正常启动初始化。新库默认 listen=3（内外网均监听）、exec=0、空代理、允许具名 POST /contact；聊天独立审批。已有库不补默认规则，原生daccess v0先备份后升级，旧0.8.2及未知格式拒绝启动并保留原文件。初始化归现有资源加载方法与模块内无状态算法，不增加结构、字段、跨模块接口或初始化账本；安装包仅提供程序与服务文件。具体行为见 Pishoo 清单和配置 API。

2026-10-08 用户要求取消 DNS 主动撤回，删除 `dns::withdraw` 及删除身份、退出时的调用。地址为空也停止发布与续期，DDNS 记录按租期自然过期；删除身份仍清除 mDNS 本机应答，远端缓存按 TTL 过期，退出仍关闭自有 mDNS 资源。不新增结构、字段或替代接口。

2026-10-08 用户进一步要求删除每日 OCSP 自动更新和 SIGHUP 重载，更新统一重启。删除 `Server::reload`、模块内 renew_ocsp/reload_profiles、信号及定时更新分支、运行中身份增删和相关测试/服务 ExecReload；启动身份校验、OCSP 缓存准备、DNS 续期与地址维护、应用退出清理保留。发布 future 不再装箱，select 直接使用下一次维护时刻，不持有额外 timer。此决定替代此前运行中重载和 OCSP 续期约定。

2026-10-08 用户确认服务端尚不返回租期，DNS 发布成功后统一每20秒续期，删除 Pishoo 的最低租期判断和按租期计算间隔的逻辑。失败或超时仍5秒后重试，空地址停止发布；既有函数签名与 ddns 缺头兼容保持不变。此决定替代此前三分之一租期与10秒间隔上限的安排。

2026-10-08 用户要求先删除 exec：移除宿主命令执行模块及其 `/exec` 路由、Server.exec_tasks、ServerConfig.exec、execute 跨模块接缝与专用错误 BackendUnavailable/Cancelled/Closed，删除 exec 接口文档及专用测试/脚本/依赖。新建 config.db 的 schema v1 仅保留 settings(listen)；已有库的旧 exec 列保留但不读取或更新，配置 API 不返回该字段并拒绝提交 exec 的 PATCH。`/exec` 不再保留为内置命名空间；不新增替代状态或接口。此前 exec 相关段落仅为历史决策记录，由本决定替代。

2026-10-08 用户要求每三天刷新 OCSP，并明确批准将已有 `Endpoint::reload(&self) -> Result<Self>` 纳入冻结接口。只刷新 OCSP；身份、配置、Lib 与证书/私钥变化仍需重启。run 使用局部72小时定时器与刷新 futures，不新增结构或成员。成功后更新监听、DNS 发布器、应用出站和 Router，失败保留旧内存凭据并记录日志。

2026-10-08 用户要求 Note 使用 SQLite note.db，在确认标准 WASI 的目录授权边界后，最终选择 `<身份目录>/db/<LibId>` 的逐 Lib 私有目录，guest 挂载为 `/db`。Note 文件为 `<身份目录>/db/note/note.db`；根级授权、配置与聊天数据库不开放，撤销此前整个 db 的共享授权。Sandbox.load_libs 复用现有 data_dir 参数，不新增结构、字段或跨模块接口；目录及挂载变更在重启后生效。此前 lib/<LibId>/data 挂载为 /data 的约定由本决定替代。

2026-10-08 用户要求按[管理命令与 API 详细设计](../pishoo/docs/management-design.md)实施；第九节的具体接口和 `/pishoo` 命名空间迁移已批准并纳入 Pishoo 清单。配置与 Lib 的 HTTP/离线命令复用存储算法，运行目录仍使用 Workspace API，服务管理使用已有 systemd/Homebrew 服务。结构、schema、执行和重启生命周期不变。

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
- 同规范化名称代表同一逻辑 Endpoint；多次 load 通过全局 Network 的同一身份连接池复用连接。Endpoint 不提供 close；stop_listening 只停止接入。Network 属于进程生命周期，不提供 shutdown。
- 当前不设计全局或逐 Endpoint 的网络传输配额，不预留配额字段、permit 或租约结构；保留流级背压、超时、Lib 执行和 exec 专用并发限制。
- 反代和 Lib 只允许 dhttp 出站：直接使用当前身份的 Endpoint，不增加 UpstreamKind、普通 HTTP 客户端或其他传输分支。
- 2026-09-27 用户批准 `tcp-mock` 构建例外及泛型 Network：默认后端仍用 QUIC；测试后端把 h3x 的双向请求流和单向控制/QPACK 流复用在回环 TCP 上。测试客户端是独立进程中的 dhttp Endpoint，不经过 HTTP/1 桥；Pishoo 在测试连接上注入匿名远端摘要。已批准的 Network/后端成员变更见 dhttp 清单；h3x 接口不变，此验证不代表 QUIC、TLS 对端认证或路径发现通过。
- 出站保留 `endpoint.get(url).header(...).await`；URL/header 使用已校验类型，解析错误立即返回。标准 HTTP Request 通过同一请求驱动发送。
- 不新增 dhttp Body 结构；复用 h3x 原生流，标准 Service 接缝仅用现成 StreamBody/UnsyncBoxBody 适配。
- 完成和取消使用流式 EOF、错误、stop、cancel 及读写 future 的结果。没有 ExchangeControl 或公开 finished。
- 身份直接复用 qtls 的 HandshakeSummary、LocalAuthority、RemoteAuthority，范围复用 qconn 的 Scope/Scopes。没有 RequestInfo 或 Peer 包装。
- 一个 WASM 文件统一称为 Lib，不另设 App；代码类型使用 Lib、LibResponseBody 和通用 Body/Error。
- 每个 Server 直接持有一个 Sandbox，集中拥有该身份的 Lib 集合、共享 WasmRuntime 引用与任务跟踪器；组件扫描、校验、版本替换、API 执行和 WASI 宿主能力均归 sandbox 模块。Lib 执行不限制并发数，不设置执行槽或 permit；Sandbox 不新增内部锁、取消信号、派生计数或策略容器，实际隔离由 Store、WasiCtx、limiter/fuel 和宿主能力实现。
- WASM 执行归 Pishoo；h3x 和 dhttp 不依赖 Pishoo 或 Wasmtime。
- WASM 不设总执行时长期限；每次调用仍受 Store 内存、fuel、WASI 宿主能力及出站 I/O 超时约束，取消和关闭仍由 supervisor 回收。
- daccess 的当前库接口是授权、审批和管理路由的依据；尽量复用 `pishoo/feat/daccess` 的集成，不兼容处按库调整。审批在当前请求中等待库返回的结果，不新增审批状态结构或后台等待任务。Lib API 登记规则见 Pishoo 清单，不建立导入账本。
- 不增加 Server 级统一请求并发限额或应用租约；静态/代理直接使用现成 Body，Lib 和 exec 各自管理实际执行资源。
- 第一版串行加载/重载，Server 直接持有 Router 和 Sandbox，Sandbox 直接持有 Lib；候选 Lib 只存于重载局部变量，不建立 ServerState、Release 或 begin_build 发布流程。
- Pishoo 启动时加载身份与配置，运行中仅在收到 SIGHUP 时扫描并串行重载；不定时轮询。`listen` 和 `ssh` 变化仍需重启。

2026-09-26 用户确认将 WASM 职责集中到 Sandbox：在已有 `lib_slots`、`tasks` 基础上迁入 Server 的 `libs`、`runtime`，Server 改为直接持有 `Sandbox`；新增组件加载、提交前复核、版本替换和 API Router 构造方法，`build_router` 接收已构造的 Lib Router。运行入口保留跨身份共享的 WasmRuntime，Server 保留身份取消、Endpoint、授权和整体 Router 发布。当次迁移保持 Invocation 与 Store 的成员及调用签名，`validate_lib` 的根级公开导出不变。字段和方法的完整签名见 [Pishoo 清单](pishoo-interfaces.md)。

2026-09-27 用户要求移除 DaemonConfig 与 Daemon 结构：`run` 以局部变量持有实例目录、Server、listener 和 WasmRuntime，保留串行重载及关闭顺序。随后用户取消实例配置文件和 TerminalPolicy：每个 Server 的 schema v1 settings 单行增加 `ssh` 0/1。用户又将第一版交互终端收缩为单命令宿主 exec：保留 `ssh` 数据库列与同名身份准入，删除终端会话、WASM shell 和平台隔离后端，Server 直接持有 exec 任务跟踪器与专用名额。接口以 [Pishoo 清单](pishoo-interfaces.md) 和 [exec 清单](exec-interfaces.md) 为准。

2026-09-27 用户确认同步当前 dhttp 无参数 Network 初始化：移除 NetworkConfig、ListenConfig 和 Pishoo 的 `network_config` 接缝，`DhttpNetwork::init()` 无参数；各 Server 在 `Endpoint.listen` 时交付自己的范围。完整接口以 [dhttp 清单](dhttp-interfaces.md) 为准。

2026-09-27 用户确认移除 Network.shutdown：Pishoo 退出先停止各 Server 监听、取消并等待自己的应用任务；全局 Network 与连接池保持到进程退出，不再承诺运行中主动关闭全部传输。接口以 [dhttp 清单](dhttp-interfaces.md) 为准。

2026-09-26 用户要求取消 Lib 并发限制：删除 `Sandbox.lib_slots`、`Invocation.permit`、`StoreData.permit`、`Invocation::new` 的 permit 参数和 `Error::Capacity`；保留单次执行的内存、fuel、出站次数及超时限制。完整签名与行为见 [Pishoo 清单](pishoo-interfaces.md)。

2026-09-27 用户要求取消 WASM 固定 30 秒总执行期限：长时间流式或等待 I/O 的调用可以继续运行，单次内存、fuel、出站次数与出站 I/O 超时仍保留；不增加 Lib 并发名额或新的状态字段。

2026-09-28 用户要求将每 2 秒扫描改为显式触发：运行中的身份、配置和 Lib 重载由 SIGHUP 发起，不增加状态容器或后台轮询任务。

2026-09-26 用户要求改用普通 `mod` 和同名 `.rs` 文件，并合并过碎的 Sandbox 文件。Pishoo 工作区库入口使用 `pishoo.rs`、`gateway.rs`，由 Cargo `[lib].path` 指定；测试同样使用普通模块。该文件组织调整允许现有成员和无状态函数在原职责范围内使用必要的 `pub(super)` 及显式导入；类型字段、函数参数和运行行为保持不变。Sandbox 的四文件划分见 [Pishoo 清单](pishoo-interfaces.md)。

2026-09-26 用户将同一整理要求扩展到 h3x 和 dhttp：合并同一类型或同一职责的实现片段，手写实现使用普通模块，库入口使用 crate 同名文件。h3x 的帧载荷、QPACK 编码和流读取实现收拢；dhttp 的 Endpoint、名称、身份、SSL 和访问策略实现按职责收拢。公开导出、结构字段、枚举载荷和方法签名保持不变；模块内部按现有调用关系调整导入和必要的父模块可见性。

## 文档清理

本仓此前的接入稿、Pishoo API 草案、数据库重设计、WASM HTTP 适配、身份沙盒、终端旧稿及旧架构图由本组文档替代。仍有效的应用规则已归入架构说明和结构清单。

README 的安装说明、CHANGELOG 的历史记录和 CONTEXT 词汇表不承担接口定义。其他仓库中的历史设计也不作为本轮三仓接口的实现依据。

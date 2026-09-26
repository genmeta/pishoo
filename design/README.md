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
| [dhttp 结构](dhttp-interfaces.md) | 独立 Endpoint、全局 Network、应用接入与立即关闭 |
| [Pishoo 结构](pishoo-interfaces.md) | 简单配置、Server/Router/Lib、daccess 接入与 WASM |
| [终端结构](terminal-interfaces.md) | 会话、线协议、子进程和平台隔离后端 |

## 冻结规则

1. 冻结范围覆盖 HTTP 网关、WASM 及远程终端。每个自定义有状态结构的私有成员也在范围内。
2. 清单列出的结构名、字段名及类型、枚举变体及载荷、方法签名、跨模块函数签名和调用归属，后续实现不得自行增加、删除或修改。
3. 方法体、局部变量、闭包及编译器生成的 async 状态可以按实现需要编写。模块内部的无状态辅助函数可拆分算法；它们不能新增跨模块接口或持久状态。
4. 不允许用 `Any`、通用属性包、未限定的 extensions、占位成员或匿名集合隐藏清单之外的状态。清单中的集合只能存其明确列出的业务内容。
5. 已复用的第三方类型按所选依赖版本使用，不复制新模型。后续依赖升级若改变冻结接口，按接口变更处理。
6. 发现清单无法满足实现时，先列出具体冲突、受影响调用和最小变更，取得用户明确同意后再修改清单及代码。不得在“顺手重构”中扩展结构。
7. 没有在结构清单中列出的能力不通过预留字段进入代码。终端平台支持情况是实施验收结果，不能通过不受限的执行路径补齐。
8. 这份基线冻结的是设计，不代表实现已经编译、联网或通过隔离测试。结构实现与行为验收分别检查。

## 当前边界

- h3x 不新增或修改结构、字段、接口。dhttp 使用已确定的 `open_bi`、`accept_bi`、`read_request`、`read_response`、`write_request`、`write_response`。
- Endpoint 独立 `load(name)`；Network 全局初始化。Endpoint 不持 Network、QUIC endpoint 或连接。
- 同规范化名称代表同一逻辑 Endpoint；close 立即取消并关闭该名称的通信，不排空、不生成精细关闭报告。临时停服使用 stop_listening。
- 当前不设计全局或逐 Endpoint 的网络传输配额，不预留配额字段、permit 或租约结构；保留流级背压、超时、Lib 执行和终端会话限制。
- 反代和 Lib 只允许 dhttp 出站：直接使用当前身份的 Endpoint，不增加 UpstreamKind、普通 HTTP 客户端或其他传输分支。
- 出站保留 `endpoint.get(url).header(...).await`；URL/header 使用已校验类型，解析错误立即返回。标准 HTTP Request 通过同一请求驱动发送。
- 不新增 dhttp Body 结构；复用 h3x 原生流，标准 Service 接缝仅用现成 StreamBody/UnsyncBoxBody 适配。
- 完成和取消使用流式 EOF、错误、stop、cancel 及读写 future 的结果。没有 ExchangeControl 或公开 finished。
- 身份直接复用 qtls 的 HandshakeSummary、LocalAuthority、RemoteAuthority，范围复用 qconn 的 Scope/Scopes。没有 RequestInfo 或 Peer 包装。
- 一个 WASM 文件统一称为 Lib，不另设 App；代码类型使用 Lib、LibResponseBody 和通用 Body/Error。
- 每个 Server 持有一个 Sandbox，直接拥有该身份 Lib 共用的 Semaphore 与任务跟踪器，负责执行准入和任务回收。准入仍直接使用标准 Semaphore；Sandbox 不新增取消信号、派生计数或策略容器，实际隔离由 Store、WasiCtx、limiter/fuel 和宿主能力实现。
- WASM 执行归 Pishoo；h3x 和 dhttp 不依赖 Pishoo 或 Wasmtime。
- daccess 的当前库接口是授权、审批和管理路由的依据；尽量复用 `pishoo/feat/daccess` 的集成，不兼容处按库调整。审批在当前请求中等待库返回的结果，不新增审批状态结构、后台等待任务或默认规则导入系统。
- 不增加 Server 级统一请求并发限额或应用租约；静态/代理直接使用现成 Body，Lib 和终端各自管理实际执行资源。
- 第一版串行加载/重载，Server 直接持有 Router 和 Lib；不建立 ServerState、Release 或 begin_build 发布流程。

2026-09-26 用户确认拆出 Sandbox：仅将 Server 的 `lib_slots`、`tasks` 迁入新类型，Server 改持 `Arc<Sandbox>`，并新增 `Sandbox::new/close/wait`；`build_router` 的两个资源参数合为 `Arc<Sandbox>`。字段和方法的完整签名见 [Pishoo 清单](pishoo-interfaces.md)。Server 保留身份取消、Endpoint、Router、Lib 集合和 Runtime，Invocation 与 Store 的成员及调用签名不变。

## 文档清理

本仓此前的接入稿、Pishoo API 草案、数据库重设计、WASM HTTP 适配、身份沙盒、终端旧稿及旧架构图由本组文档替代。仍有效的应用规则已归入架构说明和结构清单。

README 的安装说明、CHANGELOG 的历史记录和 CONTEXT 词汇表不承担接口定义。其他仓库中的历史设计也不作为本轮三仓接口的实现依据。

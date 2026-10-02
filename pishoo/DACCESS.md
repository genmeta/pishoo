# daccess 与 Workspace 同进程集成

## 当前基线

2026-10-02 按用户要求，将本地 Server/Router/Sandbox 重构 rebase 到
`pishoo/feat/daccess@9b733c5`。审批和联系人协议采用
`daccess/feat/fit-pishoo@cf8f72f4e6bedbd7c98648ffee31053cb509b395`，Cargo 使用固定 Git revision。

Server 直接拥有 Endpoint、AccessService、Workspace、Chat、Router、Sandbox 和 exec
任务资源。Workspace 与 Chat 使用普通 `workspace.rs`、`chat.rs` 模块；类型与接口以
[设计入口](../design/README.md)为准。

## 身份、审批和联系人

- 每个 profile 分别使用自己的 `db/access.db`、`db/workspace.db`、`db/chat.db`。
- 请求身份只来自 DHTTP 已验证的 HandshakeSummary，SubjectId 取证书 SKI 的 owner_hash
  文本字节。普通头和预先注入的 Visitor 不作为身份来源。
- daccess Allowed 放行业务，Denied 返回403，Reviewing 立即返回202和 status_url。
  状态查询按 Visitor 校验，获准后重试原请求；一次性决定由 daccess 消费。
- 联系人申请通过 `/workspace-api/contact-requests` 保存到本地队列，使用稳定
  application_id。接收方 `/contact` 从收到时起计算7天期限，申请方通过
  `/contact/self?application_id=...` 查询状态。批准操作在接收方本地完成。
- 查询状态的精确路径交由 daccess handler 执行记录归属检查；首次 POST /contact
  仍由接收方的访问策略决定。
- Workspace/Chat 的本地管理操作额外核对 owner 的名称和 SubjectId。

## Workspace 和 Chat

`pishoo/workspace/` 保留目标分支的 Solid/Vite 前端：联系人目录、发送申请、能力审批、
访问审批、设置和聊天。扩展/App 页面沿用分支占位状态。

`/workspace` 重定向至 `/workspace/`；深链接回退到 index.html，缺失资源404。
Cargo build script 使用 Bun 安装锁定依赖并构建 dist，资源由 Pishoo 内嵌提供。
本轮验证使用 Bun 1.4.2；旧 Bun 1.2 无法读取当前版本的 bun.lock。

`GET /std/profile` 与 `GET /std/profile/avatar` 提供最小公开资料。
`/workspace-api/context` 返回当前 profile、owner_name 和审批计数。

Chat 的远端入口只有 `POST /std/message`，受 daccess 和有效 Chat capability decision
共同约束。消息历史只读本地 chat.db；发送先入 outbox，保持 client_message_id 幂等、
重试及身份绑定。授予 Chat 只修改该联系人的精确 POST /std/message 规则，保留其他规则。

重载复用 Workspace/Chat 资源。Server.close 清空 Router，并停止、等待各模块现有 worker；
未完成投递保存在数据库中供恢复。

## 暂缓的底层接缝

用户要求底层接口稳定后再适配，因此本轮保留两个 OutboundTransport trait 和模拟传输测试，
生产 Endpoint 实现尚未装配。远端资料查询目前返回503；发送申请与聊天消息保留在队列中
等待出站接入。保留发送前核对实际连接 SubjectId 的要求。

旧 ControlPlane/H3 传输实现可在 feat/daccess@9b733c5 的两个 outbound.rs 中查阅；
原双身份网络测试保留在 `tests/deferred/workspace_network.rs`，等待后续接口对齐。
本地遗留 tcp-mock feature 与现行 dhttp 已移除的 feature 不兼容，也留待底层适配。

现有数据库需在副本上验证升级：旧 daccess Syncing=1 记录没有自动转换；Workspace/Chat
只接受各自支持的 schema 版本。此轮不修改日常 profile 数据库。

## 验证

前端可单独执行 `bun run build` 和 mock E2E。Rust 接入检查使用临时清单，仅解除
`tcp-mock = ["dhttp/tcp-mock"]` 的 feature 引用，其他生产源码和依赖保持当前工作区版本。
这验证上层编译与单元行为，不代表真实 DHTTP 出站已接通。当前结果见
[实施记录](../IMPLEMENTATION.md)。

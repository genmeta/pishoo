# pishoo Chat 模块独立化实施计划

更新时间：2026-09-24  
状态：C0/C1/C2/C3/C4/C5/C6/C7 已完成；远端 Chat 已收敛为 POST-only，本地能力状态缓存已接入

## 1. 核心决定

Chat 不是 Workspace 的子系统。Chat 是一个独立的业务模块，拥有自己的协议、存储、路由、
出站连接和能力声明；Workspace 只是把 Chat 的静态界面放进工作台，提供联系人上下文和
导航入口。

第一阶段仍在同一个 pishoo 进程内实现独立模块，不立即拆成单独 Cargo crate 或操作系统
服务。这里的“独立”首先指领域、数据、路由和依赖边界独立；等模块接口稳定后，再决定
是否抽取为 `pishoo-chat` crate 或独立服务。

```text
身份连接 / dhttp
        │
        ├── daccess
        │     ├── 联系人状态
        │     ├── access_rules
        │     └── /std/message 的授权
        │
        ├── Chat 模块
        │     ├── /std/message 远端协议
        │     ├── /chat-api 本地界面桥接
        │     ├── chat.db
        │     └── 带当前 identity 的出站消息投递
        │
        └── Workspace
              ├── 静态 ChatPage
              ├── 联系人列表/详情入口
              ├── 公开资料展示
              └── 调用 /chat-api，不拥有聊天数据
```

## 2. 当前实现需要纠正的地方

当前 M1 代码已经能工作，但所有权仍然错误：

- 早期实现曾把 Chat handler 和聊天表放在 Workspace 模块；
- `/std/message` 由 `workspace::router` 注册；
- 协议类型和能力目录位于 `workspace` 命名空间；
- 计划中的 `/workspace-api/chats/*` 会让浏览器和 Workspace 继续拥有聊天编排。

这些代码只能视为已验证的消息协议 spike。下一步先做架构迁移，再继续发送编排或聊天
页面；不能在当前 Workspace 所有权上继续堆功能。

数据库策略：不导入早期 Workspace Chat 表。ChatStore 只创建当前队列 schema；已有数据库版本
不是当前版本时在启动时拒绝，由部署流程重建 profile 数据库。

## 3. 领域边界

### 3.1 Chat 模块负责

- `/std/message` 的 POST 标准端点；
- 消息协议、正文校验、幂等键和错误码；
- profile-local `chat.db` 以及消息、会话和投递状态；
- 出站 DHTTP/mTLS 消息投递；
- 本地 `/chat-api/*` 桥接 API；
- Chat 能力 descriptor：端点、申请内容、展示名和版本；
- ChatPage 所需的消息状态和能力状态。

### 3.2 Workspace 负责

- Workspace shell、联系人列表、联系人详情和稳定路由；
- 将静态 ChatPage 打包到 `/workspace` 前端资源；
- 从联系人详情进入 `/workspace/contacts/:name/chat`；
- 展示公开资料能力的名称、头像和 fallback；
- 展示 Chat 模块返回的状态，不直接读取 Chat 数据库；
- 不接受消息正文、远端 URL、sender、recipient 或权限字段作为 Workspace 自己的安全状态。

### 3.3 daccess 负责

- DHTTP/mTLS 身份解析和 `Visitor`；
- 联系人 active/blocked/expired/subject changed 状态；
- 远端 `POST /std/message` 的 `access_rules` 授权和 review/deny 语义；
- Workspace 的 Chat grant/revoke 调用只写入固定 Chat 能力的精确规则；
- 最后一位管理员保护和内存 Policies 同步。

Chat 不复制 daccess 授权算法。Workspace 也不直接写 `access_rules`。

## 4. 路由与 API 归属

### 4.1 远端标准 API

Chat 模块独立注册：

```text
POST /std/message
```

`GET /std/message` 不属于远端 Chat 能力。接收方的消息由对方 worker 通过 `POST /std/message`
投递后写入本地 `chat.db`；本地界面通过 owner-only Chat bridge 读取本地消息。

该路径虽然使用 `/std/` 命名空间，但不属于 Workspace。它由 Chat router 提供，
通过现有 daccess middleware 授权；只有精确的 `/std/profile` 和 `/std/profile/avatar`
属于公开资料能力例外。

### 4.2 本地界面桥接 API

Workspace 中的 ChatPage 不直接访问远端 DHTTP，也不使用 `/workspace-api/chats/*`。它
调用 Chat 模块自己的 owner-only bridge：

```text
GET  /chat-api/conversations/{name}/messages?after=<cursor>&limit=<n>
POST /chat-api/conversations/{name}/messages
POST /chat-api/conversations/{name}/messages/{id}/requeue
GET  /chat-api/conversations/{name}/capability
```

`/sync` 和远端 `GET /std/message` 已从实现中移除。消息由远端 POST 投递完成，浏览器只读取
本地数据库。

`/chat-api/*` 的路由和 handler 属于 Chat 模块，Workspace 只持有前端 client 和静态页面。
路由仍可由同一个 pishoo server 提供，但不能放进 `workspace::router` 或 Workspace API
类型集合。

### 4.3 静态界面归属

第一版 ChatPage 可以继续随 Workspace 的静态 bundle 发布，这是部署便利，不改变 API 所有权：

- 静态页面不等于聊天服务；
- 页面刷新不丢失 Chat 数据；
- Chat API 可在没有 Workspace UI 的情况下被独立测试；
- 未来可以把同一个 ChatPage 挂载到应用管理或独立 Chat shell，而不迁移数据库和协议。

## 5. 能力模型

### 5.1 两类内置能力

| 能力 | 端点 | 可见性 | 是否审批 | 所属模块 |
| --- | --- | --- | --- | --- |
| `public_profile` | `GET /std/profile`、`GET /std/profile/avatar` | public | 否，直接开放 | Profile capability |
| `chat` | `POST /std/message` | contact | 是，由提供方审核申请 | Chat module |

`public_profile` 不出现在联系人申请的 capability ID、`requested_access`、`offers` 或 Chat
数据库能力状态中。后两者只用于 daccess 授权规则。它是独立公开能力，读取失败时由
Workspace 使用 identity 简称和首字母头像。

能力目录始终以当前 profile 的提供方视角展示：`public_profile` 表示当前 profile 对外公开的
资料，`chat` 表示当前 profile 可以向获准联系人开放的消息 API；“审批”指当前 profile 审核
申请方，而不是申请方审核当前 profile。

`chat` 由 Chat 模块声明，但 daccess 负责实际授权。固定声明：

```json
{
  "capability": "chat",
  "version": 1,
  "requested_access": {
    "/std/message": ["POST"]
  },
  "offers": {
    "/std/message": {
      "allow": ["POST"]
    }
  }
}
```

浏览器不能提交任意 API 路径、方法或 effect。日历、文件和项目等未来能力各自拥有
descriptor 和规则，不与 Chat 合并成一个用户可编辑的权限包。

### 5.2 能力状态的来源

- daccess 是“是否允许对方 POST `/std/message` 投递消息”的权威来源；
- Chat module 是“本地消息是否已落库、是否有 pending/failed”的权威来源；
- Workspace 只展示 Chat bridge 返回的产品状态；
- `contact_capabilities` 如果需要，放在 Chat module 的 `chat.db`，不放 Workspace DB；
- `public_profile` 永远不创建联系人能力状态。

## 6. 数据所有权与迁移

### 6.1 新的 Chat 数据库

Chat 使用 profile-local：

```text
<profile>/db/chat.db
```

Chat 自己维护 schema version，不复用 `workspace.db` 的 `module_versions`。初始表：

```text
module_versions
chat_conversations
chat_capability_state
chat_messages
chat_jobs
```

建议结构：

```text
chat_conversations
-------------------
contact_name       TEXT PRIMARY KEY
updated_at         INTEGER NOT NULL

chat_capability_state
---------------------
contact_name             TEXT PRIMARY KEY
remote_message_granted  INTEGER NOT NULL
updated_at               INTEGER NOT NULL

chat_messages
-------------
id                 INTEGER PRIMARY KEY AUTOINCREMENT
contact_name       TEXT NOT NULL
client_message_id  TEXT NOT NULL
remote_message_id  TEXT
direction          TEXT NOT NULL CHECK (direction IN ('incoming', 'outgoing'))
state              TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'sent', 'received', 'failed', 'blocked'))
sender_name        TEXT NOT NULL
recipient_name     TEXT NOT NULL
text               TEXT NOT NULL
created_at         INTEGER NOT NULL
updated_at         INTEGER NOT NULL
error_message      TEXT

UNIQUE (contact_name, client_message_id)
```

消息表的 `contact_name` 只表示会话索引，不是浏览器提交的 recipient。发送方和接收方仍
由 DHTTP 身份和当前 profile 服务器端确定。

### 6.2 Workspace 与 Chat 数据库策略

Workspace DB 使用 schema v8，Chat DB 使用 schema v4。两者只创建当前 schema，不保留旧版本
升级链或旧 Workspace Chat 表；不支持的版本在启动时拒绝。

### 6.3 Workspace DB 的最终职责

Workspace DB 只保留：profile preferences、联系人申请编排和 Workspace UI 设置。聊天正文、
投递任务、pending/failed 状态和能力状态不再写入这里。

## 7. 模块装配方式

建议结构：

```text
pishoo/src/
  chat/
    mod.rs          # ChatState、router、module manifest
    protocol.rs     # MessageSubmission、MessageEnvelope、幂等校验
    store.rs        # chat.db 与独立 migration
    service.rs      # 本地收发、幂等和联系人校验
    outbound.rs     # 带 profile identity 的远端 /std/message client
    capability.rs   # Chat descriptor
  workspace/
    mod.rs          # shell、联系人、profile、settings API
    ...
```

`management_app` 组合顺序调整为：

```text
Workspace router
  .merge(Chat router)
  .merge(daccess management router)
```

但 Chat router 的实现和状态不从 WorkspaceState 读取。建议新增独立的 `ChatState`，只注入：

- 当前 profile identity/name/subject；
- ChatStore；
- AccessService 查询接口或受限的 contact capability adapter；
- 当前 profile 的 connector factory。

ChatState 不持有 WorkspaceStore，也不调用 Workspace handler。

## 8. 本地 bridge 语义

### 8.1 读取

`GET /chat-api/conversations/{name}/messages`：

- owner-only；
- 只读取 Chat DB 中当前 profile 的会话；
- 不允许通过 path 访问未建立的第三方目标；
- 返回最近消息和本地 cursor；
- 只查询当前 profile 的 `chat.db`，不触发远端网络请求。

### 8.2 发送

`POST /chat-api/conversations/{name}/messages` 请求体只有：

```json
{"text":"你好"}
```

Chat service 负责生成 client ID、校验 active/subject/capability，并在本地事务中写入
`queued` 消息和 `send` job，立即返回本地结果。远端调用由 Chat worker 完成，成功后更新
`sent`，临时失败按退避继续排队，永久错误写入 `failed` 或 `blocked`。

`POST /chat-api/conversations/{name}/messages/{id}/requeue` 只允许把本地 failed/blocked
消息重新放回队列，复用原 client ID；它也不直接访问远端。

### 8.3 接收

远端 worker 调用本 profile 的 `POST /std/message`：

- daccess 验证 Visitor、SubjectId、联系人状态和 Chat 投递规则；
- Chat handler 按 `client_message_id` 幂等写入本地 `chat.db`；
- 返回已落库的消息结果；
- 本地 ChatPage 后续只读取本地消息，不向发送方发起读取请求。

## 9. Workspace 静态查看体验

### 9.1 路由

Workspace 仍提供：

```text
/workspace/contacts/:name/chat
```

这是查看器路由，不是 Chat API 路由。它加载静态 `ChatPage`，然后调用 `/chat-api/*`。

### 9.2 联系人详情

- 公开资料区域独立于 Chat 状态；
- 聊天按钮根据 Chat bridge 的 capability state 决定；
- pending/blocked/expired 显示原因，不打开空白聊天页；
- public profile 读取失败时仍显示 identity fallback；
- 页面不显示 `/std/message` 或 `/chat-api` 的原始路径。

### 9.3 响应式要求

- 375px：消息会话优先，顶部提供返回联系人详情；
- 768px：可使用紧凑双栏；
- 1440px：联系人上下文、消息时间线和输入区保持稳定布局；
- 输入区和发送按钮至少 44×44px；
- 所有错误、同步状态和发送状态都提供文字，不只依赖颜色；
- 使用可见 label、键盘焦点和 reduced-motion 支持。

## 10. 修订后的实施阶段

### C0：边界迁移与 ChatStore

- [x] 新增 `pishoo/src/chat/` 模块和独立 `ChatState`；
- [x] 将协议类型、能力 descriptor 和标准 handler 从 Workspace 移出；
- [x] 新增 `chat.db`、独立 migration 和 profile 隔离测试；
- [x] Workspace 与 Chat 使用全新独立 schema，不保留 Workspace 聊天表导入代码；
- [x] 从 `workspace::router` 移除 `/std/message`；
- [x] 让 `management_app` 独立 merge Chat router；
- [x] 现有 `/std/message` 行为在迁移前后保持一致。

验收：Workspace DB 不再有新的聊天写入；Chat DB 能独立启动、迁移和读取历史消息。

### C1：独立标准 Chat API

- [x] 在 Chat router 中提供 `/std/message`；
- [x] 保持 daccess 对 POST 投递的正常授权；
- [x] 保持消息正文、幂等 ID 和 64 KiB body limit；
- [x] 真实双 profile DHTTP/mTLS 测试迁移到 Chat fixture；
- [x] 验证 public profile 仍是唯一公开资料例外，不误放开 Chat。

### C2：独立 Chat bridge 与出站编排

- [x] 新增 `/chat-api/conversations/*`，不新增 `/workspace-api/chats/*`；
- [x] Chat module 自己拥有 pending/sent/failed 和重试；
- [x] 复用 control plane connector，但不依赖 WorkspaceState；
- [x] root/worker 两种 connector 的身份和错误语义一致；
- [x] 增加 owner-only、联系人状态和主体变更测试。

### C3：Workspace 静态 ChatPage

- [x] Workspace 只增加 ChatPage 静态页面和 `/workspace/contacts/:name/chat` 入口；
- [x] 前端 chat client 单独放在 Chat feature，不混入 Workspace API client；
- [x] 实现本地消息列表、发送、失败重试和状态空页面；
- [x] 增加公开资料 fallback、键盘操作和 desktop/tablet/mobile 无横向溢出测试夹具。

### C4：固定 Chat 能力与联系人生命周期

- [x] Chat module 输出 `BuiltInCapability::Chat` descriptor；
- [x] 联系人申请由用户手动选择能力，服务端接收 `capabilities` 列表；
- [x] 批准时通过 daccess `AccessService` 增量写入 `/std/message` 精确规则；
- [x] 不替换其他精确规则，不覆盖分组规则；
- [x] 出站 active reconciliation 校验远端名称和 SubjectId，并建立本地 Chat 联系人；
- [x] `public_profile` 保持公开、独立且不生成联系人能力状态；
- [x] Chat bridge 返回 available/waiting/blocked 等产品状态。

### C5：退出用户自定义权限集合

- [x] 移除权限集合 UI、联系人申请编辑器和应用按钮；
- [x] 删除 permission-set API、数据库表、迁移和所有旧权限集合代码；
- [x] 不再保留权限集合数据兼容或 410 托底；Chat grant/revoke 只通过内置 descriptor 写入 daccess 规则；
- [x] 新能力只能由内置 descriptor 注册，不允许浏览器自定义路径/effect。

### C6：内置能力目录与审计边界

- [x] 将 Chat 和公开资料能力接入统一的只读能力目录；
- [x] 为每个内置能力提供稳定版本、公开性、审批方式和固定端点元数据；
- [x] Workspace 展示能力目录和联系人能力状态，不重新引入路径/effect 编辑器；
- [x] 联系人申请可手动勾选能力（也可不选），服务端根据 descriptor 生成 requested/offers 和规则；
- [x] 为能力 descriptor 增加跨模块隔离测试，确保新增能力不会覆盖 Chat 或 public profile 规则；
- [x] 在 daccess 文档中记录内置能力与联系人生命周期的授权矩阵。

### C7：本地队列与异步投递

- [x] 将 Chat schema 重建为队列版本，加入消息投递字段和 `chat_jobs`；
- [x] 发送 API 改为本地事务入队，返回 `202 Accepted`，不在请求中访问远端；
- [x] 增加 profile-scoped Chat worker、任务 lease、重启恢复和基础退避；
- [x] Chat UI 移除普通用户手动重试，展示 queued/sending/sent/failed/blocked 状态；
- [x] 发送先本地入队，不要求先刷新远端授权；远端权限拒绝进入 blocked，重新授权后自动恢复；
- [x] 移除远端 GET、remote cursor、sync job 和 `/chat-api/.../sync`；
- [ ] 补充长时间离线、身份轮换、并发联系人和跨进程 worker 验收。

## 11. 安全和验证矩阵

| 范围 | 最低验收 |
| --- | --- |
| 所有权 | Chat API/DB 不依赖 WorkspaceStore 或 WorkspaceState |
| 标准端点 | daccess 对 POST 投递的 deny/allow、active、blocked、expired、subject mismatch |
| 本地 bridge | owner-only、目标绑定、pending/sent/failed、重试幂等 |
| 数据库 | 当前 Workspace/Chat schema 独立创建，版本不匹配直接要求重建 |
| 能力隔离 | public_profile 不审批；chat 不公开；未来能力不互相覆盖 |
| 身份安全 | 浏览器不能伪造 sender/recipient/target/subject_id |
| UI | 静态 ChatPage 不拥有数据；消息/错误/投递状态可见 |
| 响应式 | 375、768、1440px，无横向滚动 |
| root/worker | Chat outbound connector 行为一致 |

## 12. 当前下一步

当前不是继续实现 `/workspace-api/chats/*`，也不是继续扩展 Workspace DB。C0/C1/C2/C3/C4/C5
和 C7 的核心实现已经完成；身份轮换的本地消息及授权缓存隔离已接入，后续仍需补充离线、
并发联系人和跨进程 worker 验收，具体设计见 `pishoo/docs/workspace/CHAT_ASYNC_DELIVERY_DESIGN.md`。
联系人能力流程继续按 M4/M5 推进，见
`pishoo/docs/workspace/CONTACT_CAPABILITY_REQUESTS_IMPLEMENTATION_PLAN.md`。

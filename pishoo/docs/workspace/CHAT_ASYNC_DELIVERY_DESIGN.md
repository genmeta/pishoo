# Chat 本地队列与异步投递设计

更新时间：2026-09-24

## 1. 目标

Chat 的本地数据接口只负责访问当前 profile 的 `chat.db`。发送消息和查询消息都不应该因为
对方是否在线而阻塞；唯一主动访问远端 `/std/message` 的代码路径是 Chat worker 的 POST
投递。

因此，用户点击发送时，消息先以本地事务写入数据库和持久化任务队列。对方离线、网络中断、
pishoo 重启或身份热更新都由 worker 处理，浏览器只观察本地消息状态。

## 2. 边界

```text
浏览器
  -> Chat API
  -> chat.db：消息 + outbox job
  -> Chat worker
  -> 当前 profile identity 的 DHTTP connector
  -> 远端 POST /std/message
```

远端主动发送消息时仍然调用本 profile 的 `POST /std/message`。该标准 handler 只做 daccess
鉴权、幂等校验和本地落库，不再通过 Workspace 或浏览器转发。

## 3. 本地 API 语义

### 3.1 发送消息

`POST /chat-api/conversations/{name}/messages` 只验证 owner、active 联系人和本地申请的
Chat 能力，然后在一个数据库事务中插入 outgoing 消息和 `send` job，返回 `202 Accepted`。
远端授权缓存只用于界面提示；即使还没有观察到对方的授权，消息也会先进入本地队列，worker
投递被远端拒绝时再标记为 `blocked`。

### 3.2 查询消息

`GET /chat-api/conversations/{name}/messages` 只查询本地数据库。浏览器可以按 2 至 5 秒
刷新本地结果，后续也可以替换为本地 SSE/WebSocket 推送。

### 3.3 手动重试

正常 Chat UI 移除手动重试按钮。若保留 owner-only 管理接口，它只能把失败 job 重新排队，
不能在 HTTP 请求中直接访问远端。

## 4. 数据库 schema

Chat 数据库直接创建当前队列 schema，不保留旧消息状态和兼容迁移。当前 Chat schema version
为 4；版本不匹配时由部署流程重建 profile 数据库。

### 4.1 `chat_conversations`

保存联系人会话索引和本地展示状态。

```sql
contact_name TEXT PRIMARY KEY
updated_at INTEGER NOT NULL
```

### 4.2 `chat_capability_state`

```sql
contact_name TEXT PRIMARY KEY
remote_message_granted INTEGER NOT NULL CHECK (remote_message_granted IN (0, 1))
updated_at INTEGER NOT NULL
```

该表缓存最近一次 `/contact/self` 返回的远端 `POST /std/message` 授权。它只用于本地
界面提示和恢复被权限拒绝的队列，不替代接收方的 daccess 授权；远端撤销后，实际 POST
仍以接收方授权结果为准。

### 4.3 `chat_messages`

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
contact_name TEXT NOT NULL
client_message_id TEXT NOT NULL
remote_message_id TEXT
direction TEXT NOT NULL CHECK (direction IN ('incoming', 'outgoing'))
state TEXT NOT NULL CHECK (state IN (
  'queued', 'sending', 'sent', 'received', 'failed', 'blocked'
))
sender_name TEXT NOT NULL
recipient_name TEXT NOT NULL
text TEXT NOT NULL
attempt_count INTEGER NOT NULL DEFAULT 0
last_attempt_at INTEGER
next_attempt_at INTEGER
delivered_at INTEGER
error_message TEXT
created_at INTEGER NOT NULL
updated_at INTEGER NOT NULL
UNIQUE (contact_name, client_message_id)
```

`sent` 表示远端接口已经确认接受或幂等返回成功，不表示对方已经阅读。

### 4.4 `chat_jobs`

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
kind TEXT NOT NULL CHECK (kind IN ('send'))
contact_name TEXT NOT NULL
message_id INTEGER
state TEXT NOT NULL CHECK (state IN (
  'queued', 'running', 'completed', 'dead'
))
available_at INTEGER NOT NULL
attempt_count INTEGER NOT NULL DEFAULT 0
lease_token TEXT
lease_until INTEGER
last_error TEXT
created_at INTEGER NOT NULL
updated_at INTEGER NOT NULL
FOREIGN KEY (message_id) REFERENCES chat_messages(id)
```

发送任务按 `message_id` 唯一。任务需要有 `available_at`、lease 和错误字段，以便进程重启后恢复。

## 5. Worker

每个 profile 启动一个 Chat worker。`ChatState` 持有 ChatStore、AccessService、当前
OutboundTransport 和 worker shutdown handle。connector 使用 `RwLock` 动态读取，因此身份
热更新后新任务使用最新身份。

worker 使用 `Notify + 定时扫描`：

1. Chat API 插入 job 后唤醒 worker；
2. worker 定期扫描到期任务和崩溃后过期的 lease；
3. 领取任务时只在短数据库事务内写入 `running`、随机 lease token 和过期时间；
4. 网络请求在事务外执行；
5. 结果更新时再次校验 lease token，避免过期 worker 覆盖新 worker 的结果。

同一联系人只允许一个发送任务同时运行，保持消息顺序。当前第一版 worker 采用单循环领取
任务，保证状态转换简单；后续可以在保持联系人顺序的前提下按联系人分片增加有限并发。

## 6. 投递策略

### 6.1 成功

worker 使用原始 `client_message_id` 调用远端 `POST /std/message`。成功后将消息改为
`sent`，保存 `remote_message_id` 并完成 job。

### 6.2 临时错误

DNS、连接失败、超时、HTTP 5xx 和 429 都保留 `queued` 状态，增加尝试次数并计算退避时间：

```text
5 秒、15 秒、30 秒、1 分钟、5 分钟、15 分钟，之后最多每小时一次
```

临时错误不设置有限重试次数，确保对方长时间离线后消息仍会继续投递。

### 6.3 永久错误

非法消息、主体变化、联系人被删除或拉黑、幂等 ID 冲突等错误进入 `failed` 或 `blocked`，
停止自动重试并保留错误原因。远端明确拒绝 Chat 权限进入 `blocked`；之后观察到对方重新
授权时，原消息和任务会自动重新入队。

### 6.4 幂等和崩溃恢复

如果远端已保存消息但发送方在收到响应前崩溃，lease 到期后 worker 会使用同一个
`client_message_id` 重试。远端 `/std/message` 的联系人级幂等约束会返回原消息，不产生重复。

## 7. 前端状态

发送后立即清空输入框并显示本地消息：

| 状态 | 中文文案 |
| --- | --- |
| `queued` | 等待发送 |
| `sending` | 正在发送 |
| `sent` | 已发送 |
| `received` | 已接收 |
| `failed` | 发送失败 |
| `blocked` | 暂不可发送 |

网络暂时不可用时显示“消息已保存，等待发送”，只有永久错误才显示失败提示。

## 8. 安全边界

- 浏览器不能提交 sender、recipient、subject id、remote message id、状态或重试次数；
- 入队时和 worker 实际发送时都检查联系人 active 状态；远端 POST 由接收方授权层检查 Chat 投递规则；
- worker 使用 profile 当前身份，私钥和 connector 不进入浏览器；
- 联系人被停用后，worker 将任务置为 `blocked`；远端权限拒绝也进入 `blocked`，重新授权后自动恢复；
- 入站 `/std/message` 继续由 daccess 验证 Visitor、SubjectId、联系人状态和 Chat 规则。

## 9. 实施顺序

1. 直接重建 Chat schema，加入发送字段和 `chat_jobs`；
2. 把发送接口改为本地事务和 `202 Accepted`；
3. 增加 worker、lease、退避和 connector 动态读取；
4. 移除远端 `GET /std/message`、remote cursor、sync job 和同步按钮；
5. 调整 ChatPage 状态文案，移除普通用户手动重试；
6. 增加离线、重启、超时、幂等、顺序和 root/worker 测试。

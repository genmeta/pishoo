# pishoo `/std/message` 协议（v1）

状态：push-only 实施版  
更新时间：2026-09-24

这份文档定义一对一纯文本消息投递。消息历史属于各自 profile 的本地数据，不通过远端
消息读取接口复制。

## 1. 能力边界

`chat` 是联系人能力，唯一的远端标准端点是：

```text
POST /std/message
```

发送方把消息提交给自己的 Workspace。Workspace 先写入本地 Chat 数据库，再由 worker 调用
接收方的 `POST /std/message`。接收方收到 POST 后直接写入自己的本地 Chat 数据库。

公开资料是独立的 `public_profile` 能力：

```text
GET /std/profile
GET /std/profile/avatar
```

## 2. 身份与授权

- `/std/message` 通过 daccess 的正常授权流程；访问者由已验证的 DHTTP/mTLS 身份解析；
- `sender` 来自 `Visitor`，`recipient` 是当前 profile，二者都不能由请求 JSON 指定；
- 接收方只需为发送方授予 `POST /std/message`，不需要授予消息读取权限；
- 消息历史由 owner-only 的本地 `/chat-api/conversations/{name}/messages` 查询；
- 浏览器不能直接调用远端标准端点，也不能提交 sender、recipient、状态或重试字段。

## 3. 远端投递请求

```http
POST /std/message
Content-Type: application/json

{
  "client_message_id": "01J9MESSAGE0000000000000001",
  "text": "你好"
}
```

`client_message_id` 必填且长度为 1～128；`text` 必填、不能是纯空白、最多 4,000 个 Unicode
字符。请求体上限为 64 KiB，未知字段拒绝。服务端生成消息 `id` 和 `created_at`。

```json
{
  "id": "01J9SERVER0000000000000001",
  "client_message_id": "01J9MESSAGE0000000000000001",
  "sender": "alice.example.dhttp.net",
  "recipient": "bob.example.dhttp.net",
  "text": "你好",
  "created_at": 1790000000
}
```

相同联系人重复提交相同的 `client_message_id` 时返回已落库消息，不创建重复记录。接收方
落库成功后返回 `200 OK`；发送方 worker 据此将本地 outbox 消息标为 `sent`。

## 4. 本地 Chat 接口

浏览器只访问当前 profile 的本地接口：

```text
GET  /chat-api/conversations/{name}/messages
POST /chat-api/conversations/{name}/messages
POST /chat-api/conversations/{name}/messages/{id}/requeue
GET  /chat-api/conversations/{name}/capability
```

这些接口读取或写入当前 profile 的 Chat 数据库。`POST` 本地接口返回 `202 Accepted` 表示
消息已写入 outbox；后台 worker 负责远端投递。没有远端消息列表、远端游标或主动同步任务。

## 5. 状态码

| 状态 | 语义 |
| --- | --- |
| `200` | 远端 POST 已落库，或幂等返回已有消息 |
| `202` | 本地消息已写入 outbox |
| `400` | JSON、字段或文本无效 |
| `401` | 没有有效的 DHTTP/mTLS 身份 |
| `403` | 发送方未被接收方授予 POST，或联系人状态不允许投递 |
| `409` | client message ID 与已有正文冲突，或联系人主体发生冲突 |
| `413` | 请求体超过消息大小限制 |
| `429` | 频率限制 |
| `500` | 服务端存储或内部错误 |

worker 对网络错误和远端 `5xx/429` 使用同一个 `client_message_id` 重试；明确的权限拒绝
进入 `failed`/`blocked`，不会通过读取远端消息来补偿状态。

# Message 用户应用后端

Message 是普通 WASI HTTP Lib，包短名 `message`，展示为“message”。标准接收入口是 `POST /std/message`；主人管理入口为 `GET /std/message/list`、`PUT /std/message` 和 `PUT /std/message/retry?index=123`。OpenAPI 内声明的路径不含 `/std`，由宿主统一添加，guest 收到相同业务路径和原 query。

ACL、Contact 和 Package 是 Pishoo 原生服务，并非 WASM 能力。Message 只声明 `message` 访问组，能力目录 ID 为 `<publisher>/message#message`；目录来自 config.db 安装登记，授权来自原生 daccess。安装不授权；主人通过 `PATCH /std/acl/allow/{name}` 明确允许或拒绝 POST `/std/message`。联系人激活是独立操作，请求审批将决定保存为该用户对相应方法与路径的普通 ACL 规则，后续持续生效并可撤销。

Message 不保存本地授权决定或联系人副本；新库不创建 capability_decisions/capability_decision_events，既有表不读写。收件由宿主统一 ACL 决定，Lib 检查真实 caller、消息格式及幂等约束，不再增加 active 联系人或远端授权检查。除 `POST /std/message` 外，所有请求都要求真实所有者；所有写请求拒绝 `Sec-Fetch-Site: cross-site`。私有数据在 `db/<publisher>/message/message.db`，消息、会话和投递作业沿用已有字段；旧 chat 数据会通过数据库迁移补充 `media_type`。

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh build.sh --publisher YOUR_NAME.dhttp.net
```

当前版本为 0.1.1，输出 `dist/lib.wasm` 与构建用 manifest/OpenAPI。包仅含 backend 和 metadata，使用普通签名打包/商店发布流程。标准 `wasi:cli/run` 处理有界消息投递批次，直接携带 `If-Reviewed: break` 投递到对方 `/std/message`，不再查询对方 `/std/contact/status` 或维护 remote_grant。Workspace 私有目录无权访问本包数据。

`PUT /std/message` 的请求提供 `to`、`content`、`media_type` 和稳定的 `client_message_id`；`from`、`index`、`created_at` 由服务端生成，消息创建后进入投递队列。`GET /std/message/list` 支持按 `name`、`after` 和 `limit` 分页查询；失败外发消息可通过 `PUT /std/message/retry?index=123` 手动重试。后台向对方投递时直接携带 `If-Reviewed: break` 到 `/std/message`，不再查询对方 `/std/contact/status` 或维护 remote_grant。对方返回 `400 {"error":"need_review",...}` 时消息展示 reviewing，队列每 30 秒重试原请求，不消耗网络失败次数。对方批准后普通 ACL 持续允许，消息使用原 client_message_id 重试以保证幂等；明确 403 停止投递，其他网络错误沿用有界退避。联系人接受不再隐式授予聊天权限。

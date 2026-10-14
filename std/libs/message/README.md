# Message 用户应用后端

Message 是普通 WASI HTTP Lib，包短名 `message`，展示为“1v1聊天”。标准入口是 `POST /std/message`，主人管理入口为 `/std/message/context`、`/std/message/capability?name=…`、`/std/message/messages?name=…` 和 `/std/message/messages/retry`。OpenAPI 内声明的路径不含 `/std`，由宿主统一添加，guest 收到相同业务路径和原 query。

ACL、Contact 和 Package 是 Pishoo 原生服务，并非 WASM 能力。Message 只声明 `message` 访问组，能力目录 ID 为 `<publisher>/message#message`；目录来自 config.db 安装登记，授权来自原生 daccess。安装不授权；主人通过 `PATCH /std/acl/allow/{name}` 明确允许或拒绝 POST `/std/message`。联系人激活是独立操作，请求审批只批准本次请求。

Message 不保存本地授权决定或联系人副本；新库不创建 capability_decisions/capability_decision_events，既有表不读写。收件检查真实 caller、当前 active 联系人的名称与 owner hash、消息格式及幂等约束。私有数据在 `db/<publisher>/message/message.db`，消息、会话、投递作业和对端权限观察沿用已有字段。旧 chat 数据不自动迁移。

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh build.sh --publisher YOUR_NAME.dhttp.net
```

初始版本为 0.1.0，输出 `dist/lib.wasm` 与构建用 manifest/OpenAPI。包仅含 backend 和 metadata，使用普通签名打包/商店发布流程。标准 `wasi:cli/run` 处理有界消息投递批次，远端事实来自实际 `/std/contact/self`；它不创建联系人或批准 ACL。Workspace 私有目录无权访问本包数据。

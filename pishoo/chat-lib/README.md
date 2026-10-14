# Chat 独立后端

Chat 是普通 WASI HTTP Lib，挂载 `/api/chat/`，不依赖 Workspace。联系人和 ACL 继续由原生 daccess 管理；Chat 通过标准 WASI HTTP 读取这些资源，不保存联系人表或授权副本。

所有聊天数据位于 `db/<publisher>/chat/chat.db`：消息、会话、后台投递队列、对端授权观察，以及聊天能力决定与事件。Workspace 无权直接打开此目录。后台导出标准 `wasi:cli/run`，沿用实际联系人身份、有限队列批次、租约和消息幂等；首次发送前若没有对端授权观察，查询对端真实 `/std/contact/self`。

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh build.sh --publisher YOUR_NAME.dhttp.net
```

`wasmer.toml` 是构建输入，最终的签名 WEBC manifest 是包信息唯一权威。Chat 版本为 `0.2.0`，`dependencies=[]`，只映射 `/metadata`，仅含一个 `backend` Component。配套签名打包命令见 [Workspace 后端](../workspace-lib/README.md)。

主人管理接口：`/chat/context`、`/chat/capability?name=…`、`/chat/messages?name=…`、`/chat/messages/retry`、`/contacts/capabilities/{grant,deny,revoke}`、`/capability-requests?status=pending|expired`、`DELETE /approvals?kind=capability&id=…`。

远端收件：`POST /message`。请求需通过 daccess，还必须匹配真实 caller、当前原生联系人及聊天能力决定；不会从 HTTP 头接受身份。完整外部路径为 `/api/chat/message`。

卸载需先解除已安装 App/Lib 的依赖；显式卸载也保留私有数据。旧合包数据不自动迁移。

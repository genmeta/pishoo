# Workspace 用户应用后端

Workspace 是普通 WASI HTTP Lib，负责资料、头像、收藏与联系人申请投递队列。聊天已拆到 [chat-lib](../chat-lib/README.md)，前端是独立 [Workspace App](../workspace/README.md)。三者都没有系统特权。

联系人、身份绑定、ACL 和原生访问审批继续由 daccess 的 `/std/contact`、`/std/contacts`、`/std/acl` 管理。Workspace 不保存联系人表或授权副本，只通过标准 WASI HTTP 操作这些原生资源。它精确依赖同一发布者的 `chat@0.2.0`，通过 `/api/chat/` 查询能力决定、合并审批和联系人目录；不打开 Chat 的私有数据库。

私有数据为 `db/<publisher>/workspace/workspace.db` 和该目录下的 `profile/`。Chat 使用 `db/<publisher>/chat/chat.db`，保存消息、投递队列、聊天能力决定和事件。身份来自 `identity.local()/caller()` 的真实名称与 owner hash；主人业务要求完整 tuple 相等。

构建：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh build.sh --publisher YOUR_NAME.dhttp.net
```

构建输入为 `wasmer.toml`，输出到 `dist/`，不会改写源 OpenAPI 或包清单。包信息和固定运行依赖写在 `package.wapm/pishoo`，OpenAPI 仅描述业务接口；一个 Lib 仅有一个 backend atom 和 `/metadata` 映射。

生成三份独立签名包：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh ../workspace/package.sh --publisher YOUR_NAME.dhttp.net
```

需有发布者本机凭据；仅生成本地文件，不上传商店。输出为 `target/workspace-dist/default-artifacts/{chat-lib,workspace-lib,workspace-app}.webc`。从商店发布时先发布 Chat，再发布 Workspace Lib，最后发布 App，以满足精确依赖检查。

商店安装 Workspace App 会递归安装缺少的 workspace/chat 后端，复用已安装且相同的精确版本，拒绝版本冲突、同类短名冲突和依赖循环。每份制品单独原子保存；父包失败时保留已成功安装的后端。离线文件安装不联网，需依次安装：

```sh
pishoo --id YOUR_PROFILE lib install PUBLISHER/chat chat-lib.webc
pishoo --id YOUR_PROFILE lib install PUBLISHER/workspace workspace-lib.webc
pishoo --id YOUR_PROFILE app install PUBLISHER/workspace workspace-app.webc
```

入口为 `/app/workspace/#/`，业务后端为 `/api/workspace/` 和 `/api/chat/`。在线安装即时生效，离线 CLI 修改在下次加载生效。卸载 App 保留两个后端及其数据；Workspace Lib 仍依赖 Chat 时不能单独卸载 Chat。安装事实来自 config.db；启动只读取登记项，不扫描导入。

后台使用所有 Lib 共用的标准 `wasi:cli/run`，周期处理有限队列任务。事务在外部 HTTP 前提交，租约和 client_message_id 支持重试与幂等；没有专属 linker、业务 WIT、伪造握手身份或额外宿主状态。

API 保留字面量路径和 query 参数：`/context`、`/capabilities`、`/settings/profile`、`/profile`、`/profiles`、`/contact-requests`、`/contacts/saved`、`/contact-directory`、`/approvals`。聊天 UI 直接调用 Chat；Workspace 的既有能力管理接口通过标准 HTTP 委托 Chat。

旧合包制品和旧数据不自动拆分、改签或迁移。请从源码重建发布。旧 ACL 也不自动改写；聊天双方需安装 Chat 并明确申请、批准 `/api/chat/message` 的权限。

真实验收先构建两个后端，再单独运行 `workspace_user_lib_real_quic_workflow_persistence_and_uninstall --ignored`。该测试使用临时身份与生产 Server，验证联系人申请、审批、后台消息、幂等、撤销、并发、重载、数据隔离和卸载；不访问实际身份数据。

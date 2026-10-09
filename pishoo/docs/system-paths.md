# 系统 HTTP 路径

2026-10-09 起，Pishoo 与当前本地 daccess 的系统 HTTP 入口统一到 `/std`。
仅精确 `/std` 和以 `/std/` 开头的路径保留；`/std-extra`、`/standard` 等名称不保留。

| 原入口 | 当前入口 | 用途 |
| --- | --- | --- |
| `/contact`、`/contact/*` | `/std/contact`、`/std/contact/*` | 联系人申请、状态及管理 |
| `/contacts` | `/std/contacts` | 联系人列表及批量操作 |
| `/acl/*` | `/std/acl/*` | 授权规则与审批 |
| `/workspace/*` | `/std/workspace/*` | Workspace 页面与资源 |
| `/workspace-api/*` | `/std/workspace-api/*` | Workspace 管理 API、已加载 Lib 目录 |
| `/chat-api/*` | `/std/chat-api/*` | 本地聊天管理 |
| `/pishoo/*` | `/std/pishoo/*` | 配置、代理与磁盘 Lib 管理 |
| `/.pishoo/dhttp/*` | `/std/dhttp/*` | 同名身份的 DHTTP 正向代理 |
| `/file/*` | `/std/file/*` | 身份静态文件 |
| `/api/<LibId>/*` | `/std/api/<LibId>/*` | Lib 执行 |
| `/std/profile`、`/std/profile/avatar`、`/std/message` | 保持原路径 | 公开资料、远端消息投递 |

Workspace 根入口 `/std/workspace` 重定向到 `/std/workspace/`。
这是新命名空间内的页面跳转；旧系统 URL 不提供兼容别名或重定向。

## 路由与代理

系统入口继续经过 daccess 授权，状态查询和公开资料沿用原有身份与公开访问规则。
通过授权后的未注册 `/std` 路径返回404，不交给上游；已加载 Lib 的未声明路径返回404，
不支持的方法返回405。显式代理 location（包括精确匹配）不能占用任何 `/std` 路径，
与是否加载 Lib 无关。根代理 `/` 可以使用，但不能覆盖系统路由。

旧 `/file`、`/api`、`/contact`、`/acl` 等顶层入口按普通代理规则处理；
匹配代理时转发，未匹配时返回404。HA 的 `/api/websocket` 与 `/api/states`、
OpenCode 的 `/file` 无需改写为系统路径。旧 `/file` 的覆盖中间件与配置豁免已删除。

磁盘目录不迁移：`/std/file/a` 仍读取 `<identity>/file/a`。
Lib 清单仍声明组件内路径，例如 `/notes`；Pishoo 构造 `/std/api/note/notes`，
执行时移除外部前缀，guest 仍收到 `/notes`。已声明 GET/HEAD 的无尾斜杠 Lib 根入口307跳转到带尾斜杠入口，保留 query；页面直接使用 `fetch('notes')` 等相对 URL，浏览器自动拼接当前 Lib 入口，页面不需要宿主前缀或 LibId。POST 等操作及非根路径不进行此跳转。

## 升级

Pishoo 和 daccess 的源代码须一起更新并重新构建；配置、路由和 Lib 变化仍需重启。
Workspace 的构建 base、客户端 API、CLI 在线请求和联系人投递/轮询均使用新入口。
Note 页面源代码已更新，已有 Note WASM 必须重新构建、安装，再重启服务。

本次不修改数据库 schema，不自动改写已有 ACL、联系人权限声明或审批记录。
升级前备份数据库，逐项核对确属旧系统入口的规则，按表中映射设置新路径；
保留其他普通代理业务规则。尤其不能整体替换 `/api/*` 或 `/file/*`，
它们可能是 HA、OpenCode 或其他上游的有效权限。原系统规则是否保留在旧路径上，
也须按新的代理业务用途核对，避免把旧系统授权留给无关上游。

新 access 数据库默认创建 `POST /std/contact` 的 Named/Allow 规则；
已有数据库不会补回已删除的默认规则。联系人拉黑生成的拒绝规则使用 `/std/contact`，
最后一位管理员保护的作用域使用 `/std/acl` 及其子路径。

审批响应的 `status_url` 为 `/std/acl/review/{id}/status`，查询仍核对 Visitor 名称与 SubjectId。
已保存的旧审批 URL 需更新；路径迁移改变请求指纹，旧业务请求的审批决定不会自动授权新路径。
旧版本对端的联系人协议没有自动回退，需要双方同步升级。

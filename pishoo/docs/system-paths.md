# HTTP 路径

2026-10-14 起，`/std` 保留给宿主原生 API；普通安装的前端 App 使用 `/app/<name>/`，后端 Lib 使用 `/api/<name>/`。同一身份下 App、Lib 分别按 name 唯一。完整 publisher/name 继续用于签名、商店、依赖、管理资源、制品与数据目录。

| 路径 | 用途 |
| --- | --- |
| `/std/contact/*`、`/std/contacts` | 原生联系人资源 |
| `/std/acl/*` | 原生授权与审批 |
| `/std/pishoo/*` | 配置、代理、App/Lib 安装管理及已加载目录 |
| `/std/dhttp/<target>/*` | 宿主 DHTTP 正向代理 |
| `/std/file/*` | 宿主文件资源 |
| `/app/<name>/*` | 已登记前端 App |
| `/api/<name>/*` | 已登记后端 Lib |

Workspace 遵循普通应用规则，入口为 `/app/workspace/#/`，业务 API 为 `/api/workspace/*`。它的联系人、ACL、配置、商店转发仍直接调用相应原生 API。不存在原生 Workspace/Chat 业务路由或特殊权限。

已登记应用的具体挂载范围不能配置单独代理，安装时也检查已有代理并报冲突；上级 /、/api、/app 及其他未安装名称允许代理。两类写操作由 config.db 事务串行核验。

全部请求继续经过 daccess。已加载名称占有整个挂载范围；未声明操作返回404，方法不匹配返回405，不能回退给代理。未加载名称按普通代理配置处理，因此 Home Assistant 的 `/api/states`、`/api/websocket` 等仍可代理。仅精确 `/std` 和 `/std/` 子路径保留，显式代理不能占用它们；未注册 `/std` 路径返回404。

清单仍声明组件内路径，例如 `/notes`。宿主把 `/api/note/notes` 去掉挂载前缀后交给 guest `/notes`。已声明 GET/HEAD 的无尾斜杠根入口307跳转到带尾斜杠入口，保留 query，支持相对资源和请求；其他操作不重定向。

启动以 config.db 登记为准，制品须通过签名、身份、版本与依赖核对；未登记文件不会启用。卸载前端保留后端及数据，显式卸载后端也保留数据。

旧 `/std/app/*`、`/std/api/*` 不提供兼容别名。权限路径变更通过显式原生 API 按原有选择迁移，不在启动自动添加授权；普通代理的 `/api/*` 规则不能整体替换。签名制品、数据目录与完整包身份不随访问路径改变。

# HTTP 路径

`/std` 是标准协议命名空间，既包含 Pishoo 原生服务，也包含安装的普通 WASM 业务。原生服务不作为 WASM 能力登记。包类型、完整 publisher/name 身份和 App/Lib 安装操作仍保留。

| 路径 | 提供者与用途 |
| --- | --- |
| `/std/contact`、`/std/contact/*`、`/std/contacts` | 内置 daccess 联系人资源 |
| `/std/acl/*` | 内置 daccess 授权与请求审批 |
| `/std/acl/capabilities` | 内置 Pishoo 处理器读取已安装 Lib 的业务能力登记 |
| `/std/sys/package/libs/<name>`、`/std/sys/package/apps/<name>` | 按同类唯一短名查看、安装、更新与卸载；发布者仍属于包身份 |
| `/std/sys/package/*` 其他资源 | 内置包检查、商店安装与目录 |
| `/std/sys/settings`、`/std/sys/proxies` | 原生配置与代理基础设施 |
| `/std/dhttp/<target>/*`、`/std/file/*` | 宿主转发与文件基础设施 |
| `/std/<name>/` | 普通 App 的实际静态文件 GET/HEAD |
| `/std/profile`、`/std/profile/avatar`、`/std/profile/context` | 安装的 Profile Lib |
| `/std/message`、`/std/message/*` | 安装的 Message Lib，能力展示“1v1聊天” |

Lib OpenAPI 声明 `/message`、`/profile`、`/workspace/context` 等业务路径，宿主仅加 `/std`；guest 收到去掉该前缀的业务路径和原 query。App 入口按短名派生，Workspace 为 `/std/workspace/#/`。App 与同名 Lib 按实际方法和路径共同分派，不由静态目录前缀遮蔽业务操作。

安装、更新、加载均验证提供者冲突：不同 Lib 不共享同一业务路径，原生服务不能被包占用，App 的 GET/HEAD 与业务操作冲突拒绝。同路径不同方法可由 App/Lib 共同提供。不存在路径404，未声明方法405；`/std` 内不回退代理。

旧 `/app/*`、`/api/*` 正常按代理规则处理，不提供别名。Home Assistant `/api/states`、`/api/websocket` 等仍可代理。仅 `/std` 根及其路径段子路径保留，`/std-extra` 独立使用。

启动以 config.db 登记为准，验证签名、身份、版本、依赖与能力登记；未登记文件忽略。缺少或不一致能力登记时须正常重新安装，不扫描反写数据库。卸载 App 保留后端，卸载 Lib 保留数据与 ACL。路径权限不自动迁移，换提供者前明确处理旧路径规则。

具体边界及 schema 见 [std 服务与业务能力边界](std-capabilities-design.md)。

Workspace App 介绍页：`/std/workspace/#/intro`。对应 Profile Lib 公开上下文：`GET/HEAD /std/profile/context`；真实访客身份来自握手，响应 `private, no-store`，仍经过普通 daccess 授权。

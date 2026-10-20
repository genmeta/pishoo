# HTTP 路径

`/std` 是标准协议命名空间，包含 Pishoo 原生服务及已验签发布者为 `std.dhttp.net` 的 Lib。其他发布者的 Lib 使用 `/<Lib短名>` 前缀。原生服务不作为 WASM 能力登记。包类型、完整 publisher/name 身份和 App/Lib 安装操作仍保留。

| 路径 | 提供者与用途 |
| --- | --- |
| `/std/contact`、`/std/contact/*`、`/std/contacts` | 内置 daccess 联系人资源 |
| `/std/acl/*` | 内置 daccess 授权与请求审批 |
| `/std/acl/promissions` | 内置 Pishoo 处理器读取已安装 Lib 的业务能力登记 |
| `/std/sys/lib?name=NAME`、`/std/sys/app?name=NAME` | GET 数据库详情；PUT 以 JSON `{publisher,version}` 从商店安装或更新；DELETE 卸载 |
| `/std/sys/lib/list`、`/std/sys/app/list` | 仅读取数据库的已安装清单 |
| `/std/sys/listen`、`/std/sys/forward` | 原生配置与代理基础设施 |
| `/std/dhttp/<target>/*`、`/std/file/*` | 宿主转发与文件基础设施 |
| `/<name>/` | 普通 App 的实际静态文件 GET/HEAD |
| `/std/profile`、`/std/profile/avatar`、`/std/profile/context` | 安装的 Profile Lib |
| `/std/message`、`/std/message/*` | 安装的 Message Lib，能力展示“message” |

标准 Lib OpenAPI 声明 `/message`、`/profile` 等业务路径，宿主加 `/std`；其他 Lib 加 `/<Lib短名>`。普通 journal 声明 `/entries` 对外为 `/journal/entries`，声明 `/std/xxxx` 对外为 `/journal/std/xxxx`。guest 收到去掉所选挂载前缀的声明路径和原 query。App 入口按短名派生，Workspace 为 `/workspace/`。App 位于根短名路径，标准 Lib 位于 /std 下；其他 Lib 可与同名 App 按实际路径和方法共存，Lib 声明方法优先，剩余 App GET/HEAD 按静态/深链接规则处理。

安装、更新验证提供者冲突：不同 Lib 不共享同一对外业务路径，原生服务不能被包占用，显式代理不能占用已登记 App 或非标准 Lib 的根及子路径。不存在路径404，未声明方法405；`/std` 内不回退代理。

旧 `/app/*`、`/api/*` 正常按代理规则处理，不提供别名。Home Assistant `/api/states`、`/api/websocket` 等仍可代理。`/std` 根及其路径段子路径保留，已安装 App 和非标准 Lib 的 /<name> 根及子路径也由对应资源处理；上级与未安装名称仍可代理，显式代理不能落入这些范围。`/std-extra` 独立使用。

启动以 config.db 登记为准，读取已安装制品并解析、编译和装配资源，不重复安装时的签名、依赖和能力登记校验；未登记文件忽略。重新安装/更新仍须从商店取得精确版本，不扫描文件反写登记，也没有本地源包恢复入口。卸载 App 保留后端，卸载 Lib 保留数据与 ACL。路径权限不自动迁移，换提供者前明确处理旧路径规则。

包管理 POST 返回 405，原始 WEBC PUT 返回 415；name 只在 URI 中，请求 JSON 不再重复 name。新 Lib 为 `libs/std/<name>.wasm` 或 `libs/<name>.wasm`，App 为 `apps/<name>/public/`，新业务数据为 `db/<name>/`。已有 `db/<author>/<name>/` 可继续读取，不新建作者目录或自动搬迁。同类短名跨作者唯一，App/Lib 各自独立；作者不进入新磁盘路径；登记的发布身份决定 Lib 挂载前缀。安装登记仅七列，不保存源包摘要、作者指纹或本地防篡改信息。

包管理 API、登记、安装/卸载、磁盘布局及当前限制统一见[包管理审查稿](package-management.md)；源包签名和 manifest 见[发布包规范](package-spec.md)。

Workspace App 介绍页：`/workspace/intro`。对应 Profile Lib 公开上下文：`GET/HEAD /std/profile/context`；真实访客身份来自握手，响应 `private, no-store`，仍经过普通 daccess 授权。

旧普通 Lib 的 `/std/...` 入口不保留别名；已有 ACL 不自动迁移，请通过原生授权 API 显式调整。旧能力登记按作者/包名映射展示，后续安装写事务内原子更新其路径约束与登记路径；不改动 ACL。

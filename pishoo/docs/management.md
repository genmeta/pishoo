# Pishoo 管理命令与 API

资源命令操作指定身份的配置与组件文件；服务命令管理整个 Pishoo 进程。
配置修改由运行中的 Pishoo 自动应用；离线 Lib 修改保存到磁盘后，重启 Pishoo 才会加载。

## 身份与目录

使用 `--id NAME`（或 `-i NAME`）选择身份，例如：

```sh
pishoo --id alice.smith listen
pishoo --id alice.smith proxy /test 127.0.0.1:8080
```

未指定身份时读取身份 home 的 `settings.toml` 中 `[default].name`。
未设置默认身份时须显式提供 `--id`。home 默认是运行用户的 `~/.dhttp`，
Pishoo 不读取 DHTTP_HOME。Linux root 服务自动加载系统全局 home 和各账户的实际 home；
本地命令在身份所属账户下执行，例如 `sudo -u alice pishoo --id NAME enable`。
服务启停与 `lib check` 不接受 `--id`。

## 命令与 HTTP API

本机资源命令直接操作数据库与文件，服务停止时也可使用。
HTTP API 操作 URI 所指定的身份，先经过 daccess 授权，再核对已验证的名称与
证书 owner_hash 均属于该身份的所有者；请求头不能声明可信身份。

| 操作 | 命令 | HTTP API |
| --- | --- | --- |
| 查看监听设置 | `pishoo listen --id NAME` | `GET /std/sys/settings` |
| 修改监听范围 | `pishoo listen --id NAME internal` | `PATCH /std/sys/settings` |
| 查看代理列表 | `pishoo proxy --id NAME` | `GET /std/sys/proxies` |
| 查看单条代理 | `pishoo proxy --id NAME /test` | `GET /std/sys/proxies?location=%2Ftest` |
| 添加或更新代理 | `pishoo proxy --id NAME /test 127.0.0.1:8080` | `PATCH /std/sys/proxies` |
| 删除代理 | `pishoo proxy --id NAME remove /test` | `DELETE /std/sys/proxies?location=%2Ftest` |
| 清空代理 | `pishoo proxy --id NAME clear` | `PUT /std/sys/proxies`，请求体 `[]` |
| 替换代理列表 | `pishoo proxy --id NAME replace ./proxies.json` | `PUT /std/sys/proxies` |
| 列出磁盘 Lib | `pishoo lib --id NAME` | `GET /std/pishoo/libs` |
| 查看 Lib | `pishoo lib --id NAME info note` | `GET /std/pishoo/libs/note` |
| 列出已加载 Lib | `pishoo lib --id NAME --loaded` | `GET /std/workspace-api/libs` |
| 安装或更新 Lib | `pishoo lib --id NAME install note ./note.wasm` | `PUT /std/pishoo/libs/note` |
| 移除 Lib | `pishoo lib --id NAME remove note` | `DELETE /std/pishoo/libs/note` |
| 校验组件 | `pishoo lib check ./note.wasm` | `POST /std/pishoo/lib-check` |

`proxy` 和 `lib` 的列表操作也接受 `list`、`ls`；`remove` 可写为 `rm`。
`lib --loaded` 查询运行中的服务，要求服务可达，其结果可能与磁盘列表不同。

## 监听与代理

监听值为 `off`、`internal`、`external`、`both`，也接受数字 `0`、`1`、`2`、`3`。
PATCH 设置请求体示例：

```json
{"listen":1}
```

代理 PATCH 接收一个对象；PUT 接收完整数组：

```json
{"location":"/test","proxy_pass":"http://127.0.0.1:8080"}
```

上游必须是带明确端口的本机回环 HTTP/TCP 地址。
`/test` 匹配该路径及路径段子路径；`= /test` 只匹配该路径。
上游未写路径时保留原请求路径；显式 `/` 或 `/api/` 替换匹配前缀。
`/std` 及其子路径保留给系统服务，不能配置为显式代理 location。
更多载荷、路径和版本约定见[配置 API](config-api.md)及[系统路径](system-paths.md)。

## Lib 安装与数据

WEBC 安装包会展开到 `libs/<name>/` 或 `apps/<name>/`；已验签发布者为 `std.dhttp.net` 的制品放在
`libs/std/<name>/`、`apps/std/<name>/`。Lib 保存 `<name>.wasm`、OpenAPI、清单和签名，
App 保存 public 前端资源、清单和签名；隐藏 `.webc-proof` 用于复用原包完整验签。
完整包身份仍保存在签名清单与安装登记中，Lib 私有数据仍位于
`<身份目录>/db/<publisher>/<name>/`，旧数据不自动迁移。详情见[包规范](package-spec.md#6-安装更新卸载和数据)。

本机资源地址统一使用短名：

| 操作 | Lib | App |
| --- | --- | --- |
| 列出安装登记 | `GET /std/sys/package/libs` | `GET /std/sys/package/apps` |
| 查看单个制品 | `GET /std/sys/package/libs/<name>` | `GET /std/sys/package/apps/<name>` |
| 安装或更新 | `PUT /std/sys/package/libs/<name>` | `PUT /std/sys/package/apps/<name>` |
| 卸载 | `DELETE /std/sys/package/libs/<name>` | `DELETE /std/sys/package/apps/<name>` |
| 校验上传 | `POST /std/sys/package/lib-check` | `POST /std/sys/package/app-check` |
| 已加载目录 | `GET /std/sys/package/loaded-libs` | `GET /std/sys/package/loaded-apps` |

name 以小写 ASCII 字母开头，只含小写字母、数字和连字符，最长63字节。
每个身份内同类短名唯一，跨发布者同名安装返回409；App 与 Lib 可以同名。
GET/DELETE 从 config.db 解析完整包 ID；PUT 从验签后的包读取完整 ID，短名与 URL 不符返回400。
带 `<publisher>/<name>` 的管理路径返回404，不提供别名；百分号编码的路径分隔符返回400。
响应仍包含完整包 ID，更新核对原发布者及 owner_hash。磁盘、包身份、依赖与商店资源保留 publisher。

安装、更新、卸载在线即时生效，在途请求继续使用原资源；离线 CLI 修改下次加载生效。
卸载 App 保留 Lib；Lib 被 App/Lib 依赖时拒绝卸载。卸载 Lib 保留私有数据与 ACL，重复删除未安装短名返回204。
目录以登记为准，逐项核验签名、版本与依赖；Lib 损坏条目返回 `id` 和 `error`。

制品上传与校验使用 `Content-Type: application/webc`，最大64 MiB；成功返回200和JSON，删除返回204。
GET/DELETE 不接受非空 Body，未知 query 参数返回400；管理接口仍经 daccess 与同名同 owner 复核。
请求中断时查询短名资源确认保存结果，再决定是否重试。

商店安装使用 `POST /std/sys/package/store-install`，请求为
`{"kind":"lib","publisher":"alice.smith.dhttp.net","name":"note","version":"1.2.0"}`；kind 可为 app/lib，省略时按 lib。
`Content-Type: application/json`，最多4 KiB，接收期限15秒。商店仍使用完整 publisher/name 和精确版本。
宿主核验固定商店身份、发布者签名、包摘要及依赖，安装不自动设置访问规则；原始文件 PUT 不产生商店来源证明。

宿主安装包不携带业务 App/Lib，首次启动仅初始化原生服务及数据库。Pishoo 和提供 `genmeta install` 的新版 gmutils 可以按任意顺序安装；身份配置完成、运行中的 Pishoo 已加载该身份并可达后，由身份所属账户执行：

```sh
genmeta install std.dhttp.net/workspace --id NAME
```

命令调用上述商店安装 API，在线安装 Workspace 和其精确 Profile、Message 依赖，所需版本须已发布。Pishoo 不强依赖或自动调用 gmutils；系统包安装脚本不选择用户身份、不调用在线业务安装。

## 服务管理

```sh
pishoo start
pishoo stop
pishoo restart
pishoo status
```

Linux 使用已安装的 `pishoo.service`；macOS 使用当前用户的 Homebrew 服务。
Linux 包默认以 root 运行服务，系统身份目录使用 DHTTP 全局目录解析（默认 `/etc/dhttp`），
用户身份从系统账户数据库的实际 home 下 `.dhttp` 发现，不固定 `/home` 前缀。
优先使用 `getent passwd`/NSS，缺少 getent 时读取 `/etc/passwd`；支持 `/root`、`/var/home`
及管理员自定义的 home。Pishoo 不读取 DHTTP_HOME。
升级删除旧版自动生成且未经修改的 `identity-home.conf`，管理员的 unit、其他覆盖文件
和修改过的配置保留。不创建或依赖 `dhttp` 用户组，也不读取 `pishoo.conf`。
用户的身份、证书、数据库和应用数据始终保留。
这些操作影响服务加载的全部身份，权限不足或服务未安装时返回错误。
无参数运行 `pishoo` 会在前台启动：Linux root 加载上述多用户目录；其他情况加载当前账户的 `.dhttp`。
资源命令不自动重启服务；enabled、listen 和代理配置由运行进程自动应用。

## 完整示例

```sh
pishoo --id alice.smith proxy /test 127.0.0.1:8080
pishoo lib check ./note.wasm
pishoo --id alice.smith lib install note ./note.wasm
pishoo restart
pishoo --id alice.smith lib --loaded
```

查询结果输出到 stdout；身份、目录、保存提示与诊断输出到 stderr。
成功退出码为 0，输入或用法错误为 2，存储、权限、连接及服务管理错误为 1。

## Server 启用与禁用

```sh
pishoo enable --id alice.smith
pishoo disable --id alice.smith
pishoo listen --id alice.smith
```

enable/disable 只修改 settings.enabled，重复执行保持相同结果，不清空 listen、代理或用户数据。listen 查询展示保存的 enabled 和范围；listen off 只停监听，disable 则回收整个 Server。新身份在运行时自动发现，不需要重启。CLI 的成功提示表示数据库已保存；生效失败及重试在 Pishoo 日志中查看。完整行为见 [运行时生命周期](server-lifecycle.md)。

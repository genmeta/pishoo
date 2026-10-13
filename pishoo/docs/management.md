# Pishoo 管理命令与 API

资源命令操作指定身份的配置与组件文件；服务命令管理整个 Pishoo 进程。
配置和 Lib 修改保存到磁盘后，重启 Pishoo 才会生效。

## 身份与目录

使用 `--id NAME`（或 `-i NAME`）选择身份，例如：

```sh
pishoo --id alice.smith listen
pishoo --id alice.smith proxy /test 127.0.0.1:8080
```

未指定身份时读取身份 home 的 `settings.toml` 中 `[default].name`。
未设置默认身份时须显式提供 `--id`。home 默认是运行用户的 `~/.dhttp`，
可通过 `DHTTP_HOME` 指定；CLI 与系统服务必须使用同一个 home。
服务启停与 `lib check` 不接受 `--id`。

## 命令与 HTTP API

本机资源命令直接操作数据库与文件，服务停止时也可使用。
HTTP API 操作 URI 所指定的身份，先经过 daccess 授权，再核对已验证的名称与
证书 owner_hash 均属于该身份的所有者；请求头不能声明可信身份。

| 操作 | 命令 | HTTP API |
| --- | --- | --- |
| 查看监听设置 | `pishoo listen --id NAME` | `GET /std/pishoo/settings` |
| 修改监听范围 | `pishoo listen --id NAME internal` | `PATCH /std/pishoo/settings` |
| 查看代理列表 | `pishoo proxy --id NAME` | `GET /std/pishoo/proxies` |
| 查看单条代理 | `pishoo proxy --id NAME /test` | `GET /std/pishoo/proxies?location=%2Ftest` |
| 添加或更新代理 | `pishoo proxy --id NAME /test 127.0.0.1:8080` | `PATCH /std/pishoo/proxies` |
| 删除代理 | `pishoo proxy --id NAME remove /test` | `DELETE /std/pishoo/proxies?location=%2Ftest` |
| 清空代理 | `pishoo proxy --id NAME clear` | `PUT /std/pishoo/proxies`，请求体 `[]` |
| 替换代理列表 | `pishoo proxy --id NAME replace ./proxies.json` | `PUT /std/pishoo/proxies` |
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

组件位于 `<身份目录>/lib/<LibId>/lib.wasm`，私有数据位于
`<身份目录>/db/<LibId>/`。LibId 可以是旧短名，也可以是 `<publisher>/<name>`。
name 长度为 1～63 字节，以小写 ASCII 字母开头，后续可使用小写字母、数字和连字符；
publisher 必须是规范化完整 DHTTP 名称。完整 ID 使用两层目录和两段管理/API 路径，
不自动迁移旧短名的组件、数据或规则。

安装前校验组件及其 OpenAPI 清单。安装成功表示磁盘文件已保存，需重启后加载。
删除 Lib 保留其数据目录与 ACL 规则；重复删除不存在的 Lib 仍成功。
磁盘列表读取组件清单，损坏条目显示 `id` 和 `error`；列表成功不保证组件能编译。

JSON 写请求使用 `Content-Type: application/json`，最大 64 KiB；
组件上传和校验使用 `Content-Type: application/wasm`，最大 64 MiB。
写请求成功返回 200 和 JSON，删除返回 204。
请求中断时应查询资源确认结果，再决定是否重试。

商店安装使用 `POST /std/pishoo/store-install`，请求为
`{"publisher":"alice.smith.dhttp.net","name":"note","version":"1.2.0"}`，
`Content-Type: application/json`，最多 4 KiB，接收期限 15 秒。
入口在 daccess 后复核同名同 owner；宿主从固定的 `store.dhttp.net` 下载精确版本，
在发送前及响应时核对部署 owner_hash，并核验完整包摘要、大小、原始 OpenAPI、
发布者证书、签名和本机编译兼容性。下载不跟随第三方跳转，每个远端读取期限 120 秒。
已有有效签名完整 ID 的更新须保持 publisher owner_hash；证书轮换可用相同 owner。
成功返回既有磁盘目录条目，不运行 guest、不设置访问规则，仍需重启生效。
版本下架、签名或摘要不匹配时停止安装；保存结果未知时读取磁盘详情核实。
原始文件 PUT 不产生商店在线来源证明；旧无签名组件保持兼容，带签名组件校验内容完整性。

## 服务管理

```sh
pishoo start
pishoo stop
pishoo restart
pishoo status
```

Linux 使用已安装的 `pishoo.service`；macOS 使用当前用户的 Homebrew 服务。
这些操作影响服务加载的全部身份，权限不足或服务未安装时返回错误。
无参数运行 `pishoo` 会在前台启动身份 home 中的身份。
资源命令不自动重启服务。

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

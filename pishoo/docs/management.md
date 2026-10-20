# 管理 Pishoo

Pishoo 提供运行时和 HTTP API。可执行程序只支持无参数运行、`reload [--pid PID]`、`--help` 和 `--version`；原管理子命令已删除。

| 操作 | 当前入口 | 执行方式 |
| --- | --- | --- |
| 启用身份 | `genmeta listen --id NAME all` | 本机 config.db；缺失时显式创建 |
| 禁用身份 | `genmeta listen --id NAME off` | 本机 config.db；释放运行资源，保留业务数据 |
| 查看/设置监听 | `genmeta listen --id NAME [loopback\|internal\|external\|all\|off]` | 本机 config.db |
| 重载配置 | `pishoo reload`，前台使用 `pishoo reload --pid PID` | 向运行进程发送 SIGHUP |
| 代理查询/增改 | `genmeta forward --id NAME [LOCATION [TARGET]]` | GET/PATCH /std/sys/forward |
| 删除/清空/替换代理 | `genmeta forward --id NAME remove LOCATION` / `clear` / `replace FILE.json` | DELETE/PUT /std/sys/forward |
| 已安装包 | `genmeta list --id NAME`（类型开关见命令参考） | 现有制品 API |
| 包详情/移除 | `genmeta info PACKAGE` / `genmeta uninstall PACKAGE` | 现有制品 API |
| 商店安装/更新 | `genmeta install PACKAGE` / `genmeta update PACKAGE` | 同一 PUT /std/sys/{app\|lib}?name=NAME，JSON 仅 publisher/version，解析精确依赖 |
| 构建/签名 | `genmeta build PATH [--no-build]` | 本地构建签名；实际商店安装/更新执行宿主校验 |
| 发布源包 | `genmeta publish FILE.webc` | 向固定商店上传已签源包，不直接安装到宿主 |

身份开关和监听范围可在服务未启动、已禁用或 listen=off 时修改。代理与制品命令需要目标身份的 Pishoo 可达，经过同名 owner 校验及原有 daccess；不会在网络失败后自动改写本地包文件。包接口以[包管理审查稿](package-management.md)为准，命令细节见[genmeta 命令](genmeta-package.md)。

不提供 `--file` 或 Workspace 本地文件安装；包管理 POST 返回 405，原始 WEBC PUT 返回 415。安装/更新验证商店源包输入，不保存本地摘要或作者密钥指纹用于后续防篡改检查。安装表各七列：name、author、version、title、description、license、repository。同类名称跨作者唯一，App 与 Lib 可以同名；作者是元数据，不是新文件/业务目录的分层。新业务数据使用 `db/<name>/`，兼容使用已有 `db/<author>/<name>/`，不自动迁移或新建作者目录。

内嵌 OpenAPI 的新 Lib 仍受独立商店格式兼容阻塞；本轮不通过本地安装绕过，也不修改应用商店。

`genmeta listen --id NAME` 仅查询已有配置；带模式时创建缺失配置，`--global` 选择系统 home。没有 config.db 的身份不会被启动或运行时发现自动启用。gmutils apply 只在用户同意启用服务器后创建配置和欢迎页；非交互或拒绝启用时不创建。既有配置保留。

`reload` 只请求重新读取身份配置，成功发送信号不表示所有配置已生效；错误在 Pishoo 日志。沿用现有串行更新逻辑，保留无效配置对应的旧可用资源；不重新编译包或轮换证书。每秒自动发现仍保留。

Linux 使用 `systemctl start/stop/restart/status pishoo`，macOS 使用 `brew services start/stop/restart/info pishoo` 管理进程，不提供二次封装。宿主发行包只交付 Pishoo 与服务/基础设施文件；打包工具归 gmutils，不再交付 pishoo-pack。

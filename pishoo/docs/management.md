# 管理 Pishoo

Pishoo 提供运行时和 HTTP API。可执行程序只支持无参数运行、`reload [--pid PID]`、`--help` 和 `--version`；原管理子命令已删除。

| 操作 | 当前入口 | 执行方式 |
| --- | --- | --- |
| 启用身份 | `genmeta identity ensite --id NAME` | 本机 config.db；缺失时显式创建 |
| 禁用身份 | `genmeta identity dissite --id NAME` | 本机 config.db；保留监听范围和数据 |
| 查看/设置监听 | `genmeta identity listen --id NAME [off\|internal\|external\|both]` | 本机 config.db |
| 重载配置 | `pishoo reload`，前台使用 `pishoo reload --pid PID` | 向运行进程发送 SIGHUP |
| 代理查询/增改 | `genmeta server proxy --id NAME [LOCATION [TARGET]]` | GET/PATCH /std/sys/proxies |
| 删除/清空/替换代理 | `genmeta server proxy --id NAME remove LOCATION` / `clear` / `replace FILE.json` | DELETE/PUT /std/sys/proxies |
| 已安装/已加载包 | `genmeta package list [--loaded] [--type app\|lib] --id NAME` | 现有制品 API |
| 包详情/移除 | `genmeta package info\|remove PACKAGE --type app\|lib --id NAME` | 现有制品 API |
| 商店安装/更新 | `genmeta package install\|update PACKAGE --type app\|lib --id NAME` | store-install，解析精确依赖 |
| 本地文件安装 | `genmeta package install PACKAGE --file FILE.webc --type app\|lib --id NAME` | PUT 制品 API；先安装精确依赖 |
| 本地文件校验 | `genmeta package check FILE.webc --type app\|lib --id NAME` | POST app-check/lib-check |
| 构建/签名 | `genmeta package build PATH [--no-build]` | gmutils 构建与签名；发布者身份宿主 API 校验 |

身份开关和监听范围可在服务未启动、已禁用或 listen=0 时修改。代理与制品命令需要目标身份的 Pishoo 可达，经过同名 owner 校验及原有 daccess；不会在网络失败后自动改写本地包文件。HTTP 配置与制品 API 保持不变。

没有 config.db 的身份不会被启动或运行时发现自动启用。gmutils apply 只在用户同意启用服务器后创建配置和欢迎页；非交互或拒绝启用时不创建。既有配置保留。

`reload` 只请求重新读取身份配置，成功发送信号不表示所有配置已生效；错误在 Pishoo 日志。沿用现有串行更新逻辑，保留无效配置对应的旧可用资源；不重新编译包或轮换证书。每秒自动发现仍保留。

Linux 使用 `systemctl start/stop/restart/status pishoo`，macOS 使用 `brew services start/stop/restart/info pishoo` 管理进程，不提供二次封装。宿主发行包只交付 Pishoo 与服务/基础设施文件；打包工具归 gmutils，不再交付 pishoo-pack。

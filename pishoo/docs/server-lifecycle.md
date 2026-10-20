# Server 运行时生命周期

2026-10-15：本机身份配置由 gmutils 修改；`pishoo reload`（前台使用 `--pid PID`）发送 SIGHUP，立即调度串行配置重载。每秒自动发现继续保留。重载应用身份增删、listen 和代理，不重新编译 Lib 或轮换证书。

## 身份目录发现

Linux root 启动和每次定期扫描都发现 DHTTP 全局 home（默认 `/etc/dhttp`）以及
系统账户实际 home 下的 `.dhttp`。账户由 `getent passwd`/NSS 枚举；缺少 getent 时
读取 `/etc/passwd`，支持 `/root`、`/home`、`/var/home` 和其他发行版/自定义 home。
缺失的默认目录忽略，新增账户下次扫描可见。其他运行使用当前账户的 `~/.dhttp`。
Pishoo 不读取 DHTTP_HOME。本地 CLI 和打包命令使用当前账户的 `.dhttp`。
不跟随用户 home、.dhttp 或身份目录的符号链接。各身份保持原凭据、配置和数据目录。
重复规范化身份名会报告名称与两个路径，启动失败；运行中则保留所有现有 Server，
消除冲突后重试。身份移动到其他 home 时先关闭旧目录的 Server，再从新目录加载，
不会把新目录的配置应用到旧目录的资源。
Linux root 进程日志默认写入 `/etc/dhttp/logs/error.log`。

## 配置与操作

`settings.listen` 和 `ServerConfig.listen: String` 只保存 `loopback/internal/external/all/off`。off 完整停用身份，其余值直接指定监听范围；all 包含回环、内网和外网。没有独立 enabled 或隐藏的旧范围。Server 直接持有 `listener: Option<dhttp::ListenFuture>`；丢弃 future 撤销名称/Service 登记，共享连接池和在途请求继续按现有生命周期释放。
```sh
genmeta listen --id alice.smith all
genmeta listen --id alice.smith off
genmeta listen --id alice.smith internal
genmeta forward --id alice.smith /api 127.0.0.1:8080
```

同名身份及 owner 的 `PATCH /std/sys/listen` 接收 `{"listen":"MODE"}`；GET/PATCH 只返回 listen。转发接口为 `/std/sys/forward`，规则字段与授权保持。CLI/API 成功表示期望配置已保存，生效和失败记录在日志中。off 后 HTTP 入口不可达，使用本机 `genmeta listen --id NAME internal` 恢复。省略模式只查询；带模式时创建缺失配置，`--global` 选择系统 home。
## 串行资源更新

run 用现有 Server 集合、共享 WasmRuntime 和一个局部一秒 Interval 对照 dhttp-home 发现结果与 SQLite 配置。没有变化时不重建 Router、不重新编译、不触发 DNS 发布。扫描等待当前 DNS 发布批及 OCSP 刷新完成；加载、编译和最长15秒的单身份关闭也可能增加延迟，不保证一秒完成。

- 启用或新发现：已保存的 off 配置在凭据/WASM/worker 加载前跳过。启用后串行校验凭据、加载数据库及 Lib、构造 Router，监听成功后加入集合并进入 DNS 维护。缺少 `db/config.db` 的身份在凭据、OCSP、数据库和服务加载前跳过，不创建配置、不监听或发布地址。gmutils apply 仅在用户选择启用服务时创建配置；也可显式运行 `genmeta listen --id NAME all` 创建并启用。
- 禁用或移除：从集合移出，先撤销监听和 publisher，再清空 Router、关闭 Sandbox/Workspace/Chat 并等待回收；保留身份与磁盘数据。等待应用任务回收之前，立即从所有网卡/IP 对应的 mDNS 实例删除该身份的本机应答记录；关闭内网监听、切换为仅外网监听、监听恢复失败和进程退出同样清理。其他身份和共享 mDNS 查询资源保持。DDNS 只停止发布及续期，不发送公网撤回请求，已有远端记录自然过期。已进入的 HTTP/Lib 请求遵循既有资源生命周期，关闭超时明确记录。
- 代理变更：校验完整配置后替换 Router，复用 Endpoint、Sandbox、AccessService、Workspace 和 Chat。每个请求短暂克隆 Router 后释放锁，在途请求继续使用已取得的资源。
- listen 变更：先验证发布凭据和准备 publisher，然后释放旧监听并登记新范围；失败恢复原配置和原监听。恢复也失败则关闭该身份，下轮重新加载。全局 Network 及其他身份不受影响。

发现整个 home 失败时保留已加载身份。运行中一个身份的配置、数据库、凭据、Lib 或监听失败只报告并重试；无效配置保留该身份的旧可用资源。已加载身份的配置库缺失或空库不会被扫描自动重建。启动的配置/数据库/Lib 失败仍沿用既有启动错误与统一收尾规则。

本次不扫描或热替换已加载身份的磁盘 Lib、证书或私钥；Lib 的独立在线管理改动归其他 worktree。证书/私钥轮换仍需重启或明确 disable/enable 重新加载，OCSP 保持72小时更新。已加载身份的 OCSP 更新及扫描串行运行，不会把旧刷新结果应用给重新启用的 Server。

## SQLite v1 兼容

新库 settings 仅保存 TEXT listen，schema 仍为 v1。GET 只读兼容旧0/1/2/3为 off/internal/external/all；旧 enabled=0 或 listen=0 均为 off。正常启动和首次配置写入在完整验证后的事务中替换旧 settings，删除 enabled 和废弃 exec 列，代理及其他表保留。无效数据库不修补。

仅 internal/all 发布 mDNS，external/all 发布公网 DNS，loopback 不发布网络地址。停用不保存先前范围，本机 listen 命令显式选择新范围后恢复。

## 验证

配置回归覆盖五种模式、旧数字/开关迁移、旧路径404、非法输入拒绝、转发原子写入和授权。运行循环回归覆盖 off 移除 Server、配置失败保留旧资源、恢复监听和 mDNS 清理。真实网络验收单独运行 `server::network_tests::live_run_discovers_enables_disables_and_updates_proxies_without_restart`。

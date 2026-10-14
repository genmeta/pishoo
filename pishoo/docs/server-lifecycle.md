# Server 运行时生命周期

2026-10-13：启用开关、监听范围、代理规则以及身份目录增删在运行时串行应用。

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

`settings.enabled` 保存是否启动 Server，`listen` 保存范围0/1/2/3；两者独立。`ServerConfig.enabled: bool` 是持久配置的内存值。Server 直接持有实际登记资源 `listener: Option<dhttp::ListenFuture>`；丢弃该 future 撤销名称/Service 登记，不关闭共享连接池、socket 或已有请求。未监听、加载和关闭阶段没有 listener。没有额外 active、failed、registered 标志、状态容器、通知信号或后台编译协调器。

```sh
pishoo enable --id alice.smith
pishoo disable --id alice.smith
pishoo listen --id alice.smith internal
pishoo proxy --id alice.smith /api 127.0.0.1:8080
```

同名身份及 owner 的现有 `PATCH /std/pishoo/settings` 接收 `{"enabled":false}` 或 `{"enabled":true}`，并可同时提交 listen；省略字段保持原值。GET/PATCH 返回 enabled/listen。代理接口和权限保持不变。CLI/API 成功表示保存的期望配置，不代表运行时已应用；生效和失败记录在服务日志中。进程停止时本机命令仍能保存配置，启动后应用。禁用或 listen=0 后该身份的 HTTP 管理入口不可达，需要本机命令恢复。

## 串行资源更新

run 用现有 Server 集合、共享 WasmRuntime 和一个局部一秒 Interval 对照 dhttp-home 发现结果与 SQLite 配置。没有变化时不重建 Router、不重新编译、不触发 DNS 发布。扫描等待当前 DNS 发布批及 OCSP 刷新完成；加载、编译和最长15秒的单身份关闭也可能增加延迟，不保证一秒完成。

- 启用或新发现：已保存的 disabled 配置在凭据/WASM/worker 加载前跳过。启用后串行校验凭据、加载数据库及 Lib、构造 Router，监听成功后加入集合并进入 DNS 维护。新身份缺配置时沿用启动初始化：有效凭据通过后创建 enabled=true/listen=3 的 v1。
- 禁用或移除：从集合移出，先撤销监听和 publisher，再清空 Router、关闭 Sandbox/Workspace/Chat 并等待回收；保留身份与磁盘数据。mDNS 下轮维护清除本机记录，DDNS 停止续期并自然过期。已进入的 HTTP/Lib 请求遵循既有资源生命周期，关闭超时明确记录。
- 代理变更：校验完整配置后替换 Router，复用 Endpoint、Sandbox、AccessService、Workspace 和 Chat。每个请求短暂克隆 Router 后释放锁，在途请求继续使用已取得的资源。
- listen 变更：先验证发布凭据和准备 publisher，然后释放旧监听并登记新范围；失败恢复原配置和原监听。恢复也失败则关闭该身份，下轮重新加载。全局 Network 及其他身份不受影响。

发现整个 home 失败时保留已加载身份。运行中一个身份的配置、数据库、凭据、Lib 或监听失败只报告并重试；无效配置保留该身份的旧可用资源。已加载身份的配置库缺失或空库不会被扫描自动重建。启动的配置/数据库/Lib 失败仍沿用既有启动错误与统一收尾规则。

本次不扫描或热替换已加载身份的磁盘 Lib、证书或私钥；Lib 的独立在线管理改动归其他 worktree。证书/私钥轮换仍需重启或明确 disable/enable 重新加载，OCSP 保持72小时更新。已加载身份的 OCSP 更新及扫描串行运行，不会把旧刷新结果应用给重新启用的 Server。

## SQLite v1 兼容

新库 settings 只增加 enabled 整数列，严格限定0/1，schema仍为v1。旧v1缺列时，GET只读映射旧0为 enabled=false/listen=3，旧1/2/3为 enabled=true及原范围。旧0没有保存之前的范围，重新启用默认 both。正常加载或配置写事务完整校验后原子补列；原代理、旧exec列和其他数据保留，无效配置/版本不修补。enabled存在后，listen=0只停入站监听，enabled=false完全停用 Server。

旧gmutils的ensite/dissite只修改listen；完整 Server 开关请使用本版Pishoo CLI/API。gmutils仓库未在本次Pishoo worktree中修改，需要独立适配enabled语义。

## 验证

工作区回归通过；真实主循环 HTTP/3 验收通过，覆盖代理在线替换、listen 关闭/恢复、完整禁用/启用、新身份发现、单身份失败隔离、身份移除及 SIGTERM 释放监听。库测试覆盖 enabled 类型校验、部分更新、重复启停保留范围/代理/exec、旧v1只读兼容与事务补列、无效配置不补列、禁用身份跳过凭据/Lib/worker、代理扫描复用Sandbox、失效配置库不重建、发现失败保留资源及删除身份回收。真实网络验收单独运行 `server::network_tests::live_run_discovers_enables_disables_and_updates_proxies_without_restart`，使用独立测试CA/OCSP、原生QUIC/H3和本机HTTP上游，驱动实际run循环及SIGTERM。

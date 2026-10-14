# Pishoo 配置 API

配置接口由现有 dhttp Endpoint 的 H3 监听提供，参考 minidocs 的系统 API 命名与版本协商，以及当前 daccess 管理 API。每个身份操作自己的 `db/config.db`，继续使用 schema v1。

## 资源和方法

| 路径 | 方法 | 行为 | 成功响应 |
| --- | --- | --- | --- |
| `/std/pishoo/settings` | GET | 读取数据库中的 enabled/listen | 设置对象 |
| `/std/pishoo/settings` | PATCH | 更新指定设置，至少提供一个字段 | 保存后的设置对象 |
| `/std/pishoo/proxies` | GET | 无 query 读取全部；`?location=...` 精确读取一条 | 数组或单条对象 |
| `/std/pishoo/proxies` | PUT | 原子替换完整代理规则列表，空数组清除全部规则 | 保存后的规则数组 |
| `/std/pishoo/proxies` | PATCH | 添加或覆盖单个 location，其他规则不变 | 保存后的规则对象 |
| `/std/pishoo/proxies?location=...` | DELETE | 删除单条，缺失亦成功；无 location 拒绝 | 204，无 Body |

设置对象示例：

```json
{"enabled": true, "listen": 3}
```

`enabled` 仅接受 JSON 布尔值，控制是否加载整个 Server；disable 保留 listen 和代理配置。`listen` 仅接受整数 0、1、2、3，依次表示关闭、内网、外网、两者。宿主命令 exec 已移除；新库的 settings 含 enabled 和 listen，已有库的旧 exec 列保留但不读取或更新，PATCH 提交 exec 字段返回400。PATCH 省略的字段保持原值；并发更新通过 SQLite 即时事务串行执行，避免丢失其他字段的更新。

代理规则数组示例：

```json
[
  {"location": "/service/", "proxy_pass": "http://127.0.0.1:8080/"}
]
```

每条规则必须且只能包含字符串字段 `location`、`proxy_pass`。沿用路径、重复规则和回环 HTTP/TCP 上游校验。`location` 是唯一键，不增加数据库 ID。`= /path` 表示精确匹配，其余位置按路径段前缀匹配。仅 `/std` 根及子路径是系统保留路径，禁止显式代理 location 占用，包含 `/std/file` 和 `/std/api`，没有例外；`/std-extra` 等无关名称可代理。

系统静态文件使用 `/std/file/*`，磁盘仍为身份目录 `file/`。`/file` 与未被已安装名称占用的 `/api/*` 使用普通代理；HA 可以配置 `/` 或 `/api` 代理，OpenCode 可以配置 `/file` 代理。已安装 Lib 入口为 `/api/<name>`，App 为 `/app/<name>/`，已加载名称的整个挂载范围优先于代理，未声明操作404、方法不匹配405，未注册系统路径也不回退上游。配置变化由运行进程自动应用；离线 Lib 文件变化仍需重启。旧系统路径不提供兼容别名或重定向，已有 ACL 不自动改写，详见 [系统路径迁移](system-paths.md)。

裸上游地址规范化为 `http://` URI。未写路径的 `http://127.0.0.1:8080` 保留原请求路径；显式路径 `/` 或 `/api/` 替换匹配前缀，两者不会在读写中合并。

PATCH 代理接收单个规则对象；PUT 代理接收数组。location query 只用于代理 GET/DELETE，必须恰好出现一次，不接受其他 query 参数。GET/DELETE 不接受非空 Body。单条查询不存在返回404，DELETE 不会隐式清空整表。旧 `/sys` 不再提供管理别名。

写请求必须使用 `Content-Type: application/json`，可带 charset 参数。请求体最多 64 KiB。未知字段、null、错误类型、无效代理与空 PATCH 均拒绝。完整输入校验后再开始写事务，数据库失败也不会留下部分代理规则。

## 权限和版本

请求经过现有 daccess 授权层；处理器还核对 middleware 从 TLS 构造的 Visitor 与 Endpoint 同名、owner_hash 相同。即使 ACL 放行，匿名、其他身份及同名不同 owner_hash 的来访者仍不能操作配置。请求头不能声明可信身份。联系人、ACL 和审批继续使用已挂载的 daccess API。

管理 API 的权限由已验证的身份决定；网络可达范围由监听设置决定。

支持 `Accept-Versions: v1`，也支持包含 v1 的逗号分隔列表和多个同名头。省略时使用 v1，没有匹配版本返回 505。配置处理器的响应携带 `Supported-Versions: v1` 和 `Cache-Control: no-store`。

读取及写入返回 200 和 JSON，删除返回204；无效输入返回 400，身份不符返回 403，不支持的方法返回 405 并带 Allow，超大请求返回 413，非 JSON 写请求返回 415，存储错误返回 500，版本不匹配返回 505。外层 daccess 的拒绝与审批响应遵循当前库定义。

## 生效规则与 H3 调用

成功写入表示期望配置已保存，GET 展示数据库配置。运行进程每秒扫描并串行应用 enabled、listen 和代理配置，新身份目录也自动发现；不提供 SIGHUP 重载。扫描等待当前 DNS/OCSP 批结束，身份加载、编译及关闭也可能增加生效延迟；接口不承诺一秒内完成。生效和失败在服务日志中记录。详见 [运行时生命周期](server-lifecycle.md)。

使用原生 H3 示例客户端；设置 DHTTP_HOME 指向客户端身份目录，并以目标身份的同名凭据连接：

```sh
export PISHOO_CLIENT_IDENTITY=alice.dhttp.net
cargo run --locked -p pishoo --example pishoo-client -- get /std/pishoo/settings
cargo run --locked -p pishoo --example pishoo-client -- patch /std/pishoo/settings '{"listen":1}'
cargo run --locked -p pishoo --example pishoo-client -- put /std/pishoo/proxies '[{"location":"/service/","proxy_pass":"http://127.0.0.1:8080/"}]'
cargo run --locked -p pishoo --example pishoo-client -- get /std/pishoo/proxies
```

接口随身份监听提供。禁用 Server 或 listen=0 后该身份的 HTTP 管理入口不可达，使用本机 `pishoo enable --id NAME` 或 `pishoo listen --id NAME internal` 恢复相应能力。

## 启动初始化与数据库兼容

初始化由正常启动时的 `Server::load` 执行，安装脚本不遍历或修改用户身份。默认 home 为运行用户的 `~/.dhttp`，`DHTTP_HOME` 可覆盖。身份凭据加载成功后创建缺失的 `db`、`file`、`lib`、`logs`、`repo`、`templates` 和 `assets/profile` 目录；不生成证书、私钥或 `server.conf`；启动时本地 OCSP 缓存缺失或无效则获取、验证并保存，运行中每72小时刷新 OCSP，其他凭据变化仍需重启。repo/templates 此阶段仅建立目录，没有新增读取或执行能力。

新建目录在 Unix 使用0700，新建数据库0600；已有文件权限保持原样。数据库路径与应用目录必须是普通文件/目录，不能通过符号链接重定向。服务应以身份所属用户运行；systemd 部署应通过服务覆盖文件设置 User 和 DHTTP_HOME，配置变更后重启、SIGTERM 退出。Homebrew 按当前用户启动服务。

| 数据库 | 首次初始化 | 已有数据库 |
| --- | --- | --- |
| config.db | schema v1；settings 一行 enabled=1、listen=3（启用且内外网均监听）；代理为空 | 校验版本、配置和值；有效旧v1事务补enabled列，其他设置保留 |
| access.db | 由 daccess 建库，并写入 POST /std/contact、Allow、Named（**）规则 | v1直接加载；原生v0备份后由库事务升级；不补默认授权 |
| workspace.db | schema8；默认资料一行；联系人投递、收藏和能力决定为空 | 只接受当前版本及必要表结构；资料和队列保留 |
| chat.db | schema4；会话、消息、投递作业和授权观察为空 | 只接受当前版本及必要表结构；历史数据保留 |

所有者权限由 daccess 根据名称与证书 owner_hash 派生，不新增所有者联系人。具名好友申请默认允许提交，匿名仍拒绝；聊天按既有能力审批控制。已删除或更改的默认规则在重启后保持原样。

仅缺失或完全空白、无用户结构且未声明版本的 SQLite 数据库按首次初始化处理。config/Workspace/Chat 的建表、初始数据和版本在同一事务提交；已有未识别结构或不支持的版本直接报错，不执行建表修补。完整性检查失败也拒绝启动。配置 API 读取不重新创建被删除的配置库。

新的 access 数据库先在身份 db 目录下的临时目录中通过现有 daccess API 创建和写入默认规则，使用 SQLite 备份接口生成独立完整快照，校验并同步后发布到正式路径。正式文件缺失时以不覆盖已有文件的方式发布；已有空文件通过 SQLite 备份事务恢复，以保持 journal/WAL 一致。中断留下的未发布临时目录不会被当作正式库；当前调用正常退出时由 tempfile 清理自己的目录。

原生 daccess v0 升级先在临时副本验证，再生成 db/access-v0-backup-*.db，备份包含已提交的 WAL 数据；库自身的事务在原文件上完成升级，保留用户规则。后续v1启动不重复备份。旧0.8.2的 location_rule_sets/location_rules 不是该v0格式，目前无受支持转换，明确报错并保留原文件。旧 server.conf 不读取，也不删除。

四库独立初始化，不建立跨库事务或初始化标记表；全部加载成功后才注册监听。某库失败可使本次启动结束，已完成的库在下次启动被正常复用。升级失败不自动删库、重置授权或回退版本。

磁盘 Lib 管理与离线命令见[管理命令与 API](management.md)。本机资源命令不启动 Server，也不自动重启。

### enabled 的 v1 兼容

不增加 schema v2。GET 只读兼容缺少 enabled 列的有效 v1：旧 listen=0 返回 enabled=false/listen=3，旧 1/2/3 返回 enabled=true并保留范围；旧0未保存之前的范围，因此启用默认 both。首次正常加载或配置写入在 SQLite 即时事务中完整验证并补 enabled 列，保留代理、旧 exec 列及其他数据。无效版本、损坏配置或写入失败不提交补列。已有 enabled 列严格校验为整数0/1；enabled=true/listen=0仍可运行出站 worker，enabled=false则不加载 Server。

旧版 gmutils 的 ensite/dissite 仍只写 listen，不能操作新增的完整 Server 开关；本版使用 Pishoo 的 enable/disable CLI 或 settings API。gmutils 适配需要使用同一 enabled 语义。

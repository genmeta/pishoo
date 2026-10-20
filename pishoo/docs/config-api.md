# Pishoo 配置 API

配置接口由现有 dhttp Endpoint 的 H3 监听提供，参考 minidocs 的系统 API 命名与版本协商，以及当前 daccess 管理 API。每个身份操作自己的 `db/config.db`，继续使用 schema v1。

配置路径统一使用 `/std/sys/settings` 和 `/std/sys/proxies`，旧 `/std/pishoo` 配置路径不提供别名；已有路径权限规则不自动迁移。

## 资源和方法

| 路径 | 方法 | 行为 | 成功响应 |
| --- | --- | --- | --- |
| `/std/sys/settings` | GET | 读取数据库中的 enabled/listen | 设置对象 |
| `/std/sys/settings` | PATCH | 更新指定设置，至少提供一个字段 | 保存后的设置对象 |
| `/std/sys/proxies` | GET | 无 query 读取全部；`?location=...` 精确读取一条 | 数组或单条对象 |
| `/std/sys/proxies` | PUT | 原子替换完整代理规则列表，空数组清除全部规则 | 保存后的规则数组 |
| `/std/sys/proxies` | PATCH | 添加或覆盖单个 location，其他规则不变 | 保存后的规则对象 |
| `/std/sys/proxies?location=...` | DELETE | 删除单条，缺失亦成功；无 location 拒绝 | 204，无 Body |

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

使用原生 H3 示例客户端，在客户端身份所属账户下运行，并以目标身份的同名凭据连接：

```sh
export PISHOO_CLIENT_IDENTITY=alice.dhttp.net
cargo run --locked -p pishoo --example pishoo-client -- get /std/sys/settings
cargo run --locked -p pishoo --example pishoo-client -- patch /std/sys/settings '{"listen":1}'
cargo run --locked -p pishoo --example pishoo-client -- put /std/sys/proxies '[{"location":"/service/","proxy_pass":"http://127.0.0.1:8080/"}]'
cargo run --locked -p pishoo --example pishoo-client -- get /std/sys/proxies
```

接口随身份监听提供。禁用 Server 或 listen=0 后该身份的 HTTP 管理入口不可达，使用本机 `genmeta identity ensite --id NAME` 或 `genmeta identity listen --id NAME internal` 恢复相应能力。

## 启动初始化与数据库兼容

启动和运行时新身份发现先检查 `db/config.db`；缺失时跳过该身份，不创建配置、加载凭据或发布地址。配置由 gmutils 在用户选择启用服务后创建。其余初始化由正常启动时的 `Server::load` 执行，安装脚本不遍历或修改用户身份。Linux root 运行扫描 DHTTP 全局目录（默认 `/etc/dhttp`）和系统账户实际 home 下的 `.dhttp`，不固定用户 home 前缀。其他运行使用当前账户的 `~/.dhttp`。Pishoo 不读取 DHTTP_HOME。已确认存在配置的身份加载时创建缺失的 `db`、`file`、`libs/std`、`apps`、`logs`、`repo` 和 `templates` 目录；不生成证书、私钥或 `server.conf`；启动时本地 `ssl/stapling.ocsp` 缓存缺失或无效则获取、验证并保存，运行中每72小时刷新 OCSP，其他凭据变化仍需重启。repo/templates 此阶段仅建立目录，没有新增读取或执行能力。

新建目录在 Unix 使用0700，新建数据库0600；已有文件权限保持原样。数据库路径与应用目录必须是普通文件/目录，不能通过符号链接重定向。Linux systemd 服务默认以 root 运行并加载多用户身份；管理员可通过服务覆盖文件修改 User；非 root 运行只加载当前账户的 `.dhttp`。服务用户修改后重启，SIGTERM 退出。Homebrew 按当前用户启动服务。

root 服务创建上述身份子目录、安装或更新 App/Lib 时，目录与展开制品文件的 UID/GID 继承身份目录所有者。身份加载时同时修复已有 `libs/`、`apps/` 下常规文件及目录的归属，不改变权限位、制品内容或安装登记；符号链接、多硬链接文件及其他文件系统内的内容不参与递归修复。该规则不改变 systemd 的 root 运行身份，也不将发布者或来访者映射为系统账户。

| 数据库 | 首次初始化 | 已有数据库 |
| --- | --- | --- |
| config.db | 缺失时不自动创建；已有空库恢复为 schema v1；settings 一行 enabled=1、listen=3（启用且内外网均监听）；代理为空 | 校验版本、配置和值；有效旧v1事务补enabled列，其他设置保留 |
| access.db | 由 daccess 建库，写入 POST /std/contact、Allow、Named（**），GET /std/profile 和 HEAD /std/profile/context 的 Allow、All（*?），以及 GET /std/acl/capabilities、GET/HEAD /std/workspace 的 Allow、All（*?）规则 | v1直接加载；原生v0备份后由库事务升级；不补默认授权 |
| contact.db | 原生 Contact 首次使用时建立 schema v1，收藏和出站申请为空 | 校验现行版本及必要表，保留收藏和投递队列 |

所有者权限由 daccess 根据名称与证书 owner_hash 派生，不新增所有者联系人。具名好友申请默认允许提交，匿名仍拒绝；消息依赖联系人状态和原生 ACL 授权。已删除或更改的默认规则在重启后保持原样。

仅缺失或完全空白、无用户结构且未声明版本的 SQLite 数据库按首次初始化处理。config/Contact 的建表、初始数据和版本在同一事务提交；已有未识别结构或不支持的版本直接报错，不执行建表修补。完整性检查失败也拒绝启动。配置 API 读取不重新创建被删除的配置库。

新的 access 数据库先在身份 db 目录下的临时目录中通过现有 daccess API 创建和写入默认规则，使用 SQLite 备份接口生成独立完整快照，校验并同步后发布到正式路径。正式文件缺失时以不覆盖已有文件的方式发布；已有空文件通过 SQLite 备份事务恢复，以保持 journal/WAL 一致。中断留下的未发布临时目录不会被当作正式库；当前调用正常退出时由 tempfile 清理自己的目录。

原生 daccess v0 升级先在临时副本验证，再生成 db/access-v0-backup-*.db，备份包含已提交的 WAL 数据；库自身的事务在原文件上完成升级，保留用户规则。后续v1启动不重复备份。旧0.8.2的 location_rule_sets/location_rules 不是该v0格式，目前无受支持转换，明确报错并保留原文件。旧 server.conf 不读取，也不删除。

四库独立初始化，不建立跨库事务或初始化标记表；全部加载成功后才注册监听。某库失败可使本次启动结束，已完成的库在下次启动被正常复用。升级失败不自动删库、重置授权或回退版本。

磁盘 Lib 管理与离线命令见[管理命令与 API](management.md)。本机资源命令不启动 Server，也不自动重启。

### enabled 的 v1 兼容

不增加 schema v2。GET 只读兼容缺少 enabled 列的有效 v1：旧 listen=0 返回 enabled=false/listen=3，旧 1/2/3 返回 enabled=true并保留范围；旧0未保存之前的范围，因此启用默认 both。首次正常加载或配置写入在 SQLite 即时事务中完整验证并补 enabled 列，保留代理、旧 exec 列及其他数据。无效版本、损坏配置或写入失败不提交补列。已有 enabled 列严格校验为整数0/1；enabled=true/listen=0仍可运行出站 worker，enabled=false则不加载 Server。

旧版 gmutils 的 ensite/dissite 仍只写 listen，不能操作新增的完整 Server 开关；本版使用 Pishoo 的 enable/disable CLI 或 settings API。gmutils 适配需要使用同一 enabled 语义。

Profile 和 Message 的数据库在各自 Lib 私有目录中按使用初始化；Workspace 仅为前端 App。旧 workspace.db、chat.db 保留，不再由原生启动初始化。

### 加好友前的默认访问

新身份通过现有 daccess 初始化以下默认规则。规则可由管理员修改或移除，
重启不覆盖已有权限选择；安装业务包本身不新增授权。

| 请求 | 默认访问 | 用途 |
| --- | --- | --- |
| GET/HEAD /std/workspace | 所有访问者（含匿名）；前缀覆盖静态资源 | 保留 Workspace 的公开名片页面 `#/intro` |
| GET /std/acl/capabilities | 所有访问者（含匿名） | 发现可申请的业务能力 |
| GET /std/profile | 所有访问者（含匿名） | 读取对方公开资料 |
| GET /std/profile/avatar | 继承 GET /std/profile 前缀规则，允许匿名 | 显示头像；未设置头像可返回 404 |
| GET /std/profile/context | 继承 GET /std/profile 前缀规则，允许匿名 | 资料上下文与真实访客身份 |
| HEAD /std/profile/context | 所有访问者（含匿名） | 公开名片上下文响应头 |
| POST /std/contact | 已认证具名身份（**） | 提交联系人申请，匿名拒绝 |
| GET /std/contact/self | 处理器按已验证 Visitor 和申请编号校验 | 仅查询自己的申请/授权状态，不增加公开 ACL |

加好友页面先并行读取目标 profile 与 capabilities，再按 avatar_url 读取头像。
申请发送到本机 POST /std/contact/local/requests，由 Pishoo 使用自己的身份投递到
对方 POST /std/contact。批准后申请方轮询 GET /std/contact/self。

`/std/dhttp/*` 是本机所有者代理；`/std/contact/local/*`、联系人列表/审批和 ACL
管理接口保持管理权限。Profile 的 PATCH、头像 PUT/DELETE 不开放。
`POST /std/message` 仍需双方明确授权，联系人获批不等于聊天能力获批。
公开名片前端继续由 Workspace App 提供，Profile Lib 提供资料数据。`#/intro` 是浏览器片段，不会发送到服务器，因此 GET/HEAD 规则覆盖 Workspace 静态页面和资源，不能只按片段授权。加载静态界面不授予后台管理权限；其他方法和相邻前缀仍受限。

已有身份保留原配置，需要采用这些默认值时，在目标身份上显式执行：

```sh
genmeta access --id NAME /std/acl/capabilities allow '*?' --method GET
genmeta access --id NAME /std/profile allow '*?' --method GET
genmeta access --id NAME /std/profile/context allow '*?' --method HEAD
genmeta access --id NAME /std/contact allow '**' --method POST
genmeta access --id NAME /std/workspace allow '*?' --method GET
genmeta access --id NAME /std/workspace allow '*?' --method HEAD
```

这些管理 API 修改立即生效，无需重启。新的代码默认值在更新后的 Pishoo
初始化新身份时生效；仅重新编译不会改变正在运行的旧进程。


标准 `std.dhttp.net/workspace` App 与 `std.dhttp.net/profile` Lib 首次安装时，
允许复用上述精确的只读 Allow/All 默认规则，以免默认公开权限反过来阻止安装。
这不豁免其他发布者、自定义规则、通配方法或写操作的旧权限冲突检查。

2026-10-20：个人身份以所属账户 home 的 UID/GID 修复 .dhttp、身份和各子目录归属；不再创建 apps/std。宿主创建的数据库、访问日志、OCSP 与制品临时内容在发布时归身份账户。数据库字段和包管理路径见 [本轮契约](../../design/package-management-2026-10-20.md)。

# Pishoo 管理命令与 API 详细设计

日期：2026-10-08。状态：2026-10-08 用户已要求按本文实施。

Pishoo 统一提供配置、Lib 安装移除和本机服务管理命令。配置与 Lib 操作属于指定身份的 Server；服务启停属于整个 Pishoo 进程。每个资源操作均给出对应 API，本机服务操作对应系统服务管理器接口。

管理 API 统一使用 `/pishoo` 前缀，与 `pishoo` 命令名称一致。现有实现中的 `/sys/settings` 和 `/sys/proxies` 在实施时迁移为 `/pishoo/settings` 和 `/pishoo/proxies`；本文后续表格和示例均使用目标路径，不能据此认为新路径已经上线。

本文第九节的具体契约变更已由用户的实现请求批准并纳入冻结清单；实现结果以测试及实施记录为准。现有行为以 [设计入口](../../design/README.md)、[Pishoo 冻结接口](../../design/pishoo-interfaces.md)和[配置 API](config-api.md)为准。新增 HTTP 路由及所需跨模块函数列于第九节，并已按本次请求纳入冻结清单。

## 一 命令范围与身份选择

### 命令形式

```text
pishoo
pishoo <operation>
pishoo <resource> [--id NAME] [arguments]
```

`pishoo` 无参数继续前台运行所有启动时发现的身份。`listen`、`proxy`、`lib` 是资源命令；`start`、`stop`、`restart`、`status` 是整个服务的操作。没有 `server NAME` 子命令层级。

`--id NAME` 选择 Server 身份，短参数为 `-i`。支持写在资源命令前后，例如以下两条命令等价：

```sh
pishoo --id alice.smith proxy /test 127.0.0.1:8080
pishoo proxy --id alice.smith /test 127.0.0.1:8080
```

`--id` 仅用于需要选择 Server 的资源命令。前台运行、服务启停和离线组件校验不接受该参数，防止把身份选择误解为进程启动过滤或单 Server 重启。

### 默认身份与目录

本机管理命令通过现有 `DhttpHome::load(HomeScope::User)` 解析目录：优先 `DHTTP_HOME`，否则使用运行用户的 `~/.dhttp`。服务进程与管理命令必须指向同一身份目录；CLI 输出所选规范化身份和配置目录，写入提示输出到 stderr。

选择身份的顺序如下：

1. 有 `--id` 时，复用现有名称规范化与身份目录解析规则，定位准确的身份目录。
2. 没有 `--id` 时，读取该 home 下 gmutils 已使用的 `settings.toml` 中的 `default.name`。
3. 未设置默认身份、默认身份不存在或配置文件无法解析时，报告原因，提示显式提供 `--id`；不选择目录中的第一个身份。

共享文件的相关内容为：

```toml
[default]
name = "alice.smith.dhttp.net"
```

Pishoo 只读取这个默认选择，不创建、修改或迁移 `settings.toml`，也不把它作为进程运行配置。当前 Pishoo 使用的 dhttp-home 尚无 gmutils 的 settings 读取接口；实现须用标准 TOML 值在命令局部读取该字段，不复制 gmutils 的有状态配置结构。

`--id` 是 Server 身份；`lib install note` 中的 `note` 是 Lib 标识，二者不能混用。第一版不增加新的 home 参数、实例配置或默认身份写入命令。

### 本机命令与 HTTP API

资源命令默认操作本机数据库与文件，调用 Pishoo 内部的同一校验和存储算法，不要求 Pishoo 正在监听。HTTP 客户端通过现有 H3 Endpoint 调用对应 API；URI 的目标身份确定 Server，路径中不再重复 Server 名称。

本机命令依赖操作系统用户的文件访问权限；HTTP 请求依赖 daccess 和已验证的握手身份。本机命令不伪造 Visitor、不绕过 HTTP 中间件调用路由，也不通过失败后自动切换操作目标的方式实现离线管理。

这一区别保证 `listen=0` 或服务停止时仍能恢复本机配置。第一版不增加远程写操作的 CLI 模式；`lib --loaded` 是明确要求连接运行服务的读取例外，其他客户端可以直接使用下文 API。

## 二 操作与 API 总表

下表列出已批准并实施的接口。状态一栏标明原有 HTTP 能力的迁移与本次新增行为；“对应”表示同一资源行为，本机命令不必发出 HTTP 请求。

“现有能力迁移路径”表示复用已有的校验、载荷和存储行为，HTTP 路径随本设计调整。已加载目录仍复用既有 Workspace API，其业务接入不属于这次管理路径迁移。

| 操作 | 命令示例 | 对应 API | API 状态 |
| --- | --- | --- | --- |
| 查看监听设置 | `pishoo listen --id alice.smith` | `GET /pishoo/settings` | 现有能力迁移路径 |
| 修改监听范围 | `pishoo listen --id alice.smith internal` | `PATCH /pishoo/settings` | 现有能力迁移路径 |
| 查看全部代理 | `pishoo proxy --id alice.smith` | `GET /pishoo/proxies` | 现有能力迁移路径 |
| 查看单条代理 | `pishoo proxy --id alice.smith /test` | `GET /pishoo/proxies?location=%2Ftest` | 迁移路径并新增查询语义 |
| 添加或更新代理 | `pishoo proxy --id alice.smith /test 127.0.0.1:8080` | `PATCH /pishoo/proxies` | 新增 |
| 删除单条代理 | `pishoo proxy --id alice.smith remove /test` | `DELETE /pishoo/proxies?location=%2Ftest` | 新增 |
| 清空全部代理 | `pishoo proxy --id alice.smith clear` | `PUT /pishoo/proxies`，请求体 `[]` | 现有能力迁移路径 |
| 替换全部代理 | `pishoo proxy --id alice.smith replace ./proxies.json` | `PUT /pishoo/proxies`，请求体为完整数组 | 现有能力迁移路径 |
| 列出磁盘上的 Lib | `pishoo lib --id alice.smith` | `GET /pishoo/libs` | 新增 |
| 查看磁盘上的单个 Lib | `pishoo lib --id alice.smith info note` | `GET /pishoo/libs/note` | 新增 |
| 查看当前已加载的 Lib | `pishoo lib --id alice.smith --loaded` | `GET /workspace-api/libs` | 已有 HTTP API；本命令需要在线请求 |
| 安装或更新 Lib | `pishoo lib --id alice.smith install note ./note.wasm` | `PUT /pishoo/libs/note` | 新增 |
| 移除 Lib | `pishoo lib --id alice.smith remove note` | `DELETE /pishoo/libs/note` | 新增 |
| 校验本地 WASM 文件 | `pishoo lib check ./note.wasm` | `POST /pishoo/lib-check` | 新增；本机校验不需要身份 |
| 前台运行 | `pishoo` | 进程入口，调用现有 `run()` | 已有进程入口 |
| 启动服务 | `pishoo start` | 系统服务管理器的启动操作 | 新增命令，无 Pishoo HTTP API |
| 停止服务 | `pishoo stop` | 系统服务管理器的停止操作 | 新增命令，无 Pishoo HTTP API |
| 重启服务 | `pishoo restart` | 系统服务管理器的重启操作 | 新增命令，无 Pishoo HTTP API |
| 查看服务状态 | `pishoo status` | 系统服务管理器的状态查询 | 新增命令，无 Pishoo HTTP API |

`proxy` 和 `lib` 无操作参数时默认列出资源，也接受 `list`，别名 `ls`。`remove` 的别名是 `rm`。每种别名对应相同 API 与行为，不增加独立 handler。

## 三 HTTP 通用约定

### 身份与权限

所有新增 `/pishoo` 请求先经过现有 daccess 授权，再核对可信 Visitor 的名称与 SubjectId：须与 Server Endpoint 同名且 owner_hash 相同。即使 ACL 允许，匿名、其他名称和同名不同 owner 的请求仍返回403。请求头不能声明可信身份。

HTTP API 不承诺仅本地网络可访问。其可达范围仍取决于 Server 的监听配置；权限来自经过验证的身份。

### 版本与内容类型

沿用 `Accept-Versions: v1` 协商。省略使用 v1；多头或逗号分隔列表包含 v1 即接受，无匹配返回505。所有管理 handler 的响应包含 `Supported-Versions: v1` 和 `Cache-Control: no-store`；外层 daccess 生成的响应仍遵循库本身的行为。

JSON 写入要求 `Content-Type: application/json`，允许 charset，最多64 KiB。组件上传与校验要求 `Content-Type: application/wasm`，原始二进制 Body，最多64 MiB。读取时累计实际字节数，不能只信任 Content-Length；超过上限立即返回413。

Lib 标识必须符合现有规则：长度1至63字节，以小写 ASCII 字母开头，后续仅允许小写字母、数字、连字符。直接使用现有名称，不能包含路径分隔符、点号或百分号；路径解码后再校验。

`/pishoo/lib-check` 单独放在集合外，避免与合法 Lib 标识 `check` 的资源路径冲突。

### 返回值与错误

现有成功 JSON 形状保持不变。新增资源查询与写入返回200和 JSON；删除返回204，无 Body。安装首次创建与更新统一返回200，成功含义是磁盘发布完成。错误响应沿用配置 API 的文本形式，不增加错误 DTO 或持久错误记录。

| 状态码 | 适用情况 |
| --- | --- |
| 400 | 无效参数、未知字段、错误类型、无效代理、组件或宿主兼容性校验失败 |
| 403 | 身份不符合管理要求 |
| 404 | 查询的代理或 Lib 不存在 |
| 405 | 不支持的方法，附带 Allow |
| 409 | 目标目录或文件类型不符合资源布局，不能安全执行修改 |
| 413 | 请求体超过上限 |
| 415 | 写入 Content-Type 不支持 |
| 500 | 存储、编译基础设施或任务执行故障；响应不暴露凭据和本机绝对路径 |
| 505 | API 版本无匹配 |

输入组件导致的校验错误在对应 handler 局部转换为400。409由 handler 按具体路径冲突直接构造响应，不修改全局 `Error::status`，也不新增 Error 变体。删除不存在的资源返回204，以便重复执行。

请求中断或响应丢失不代表写入一定未提交。客户端通过 GET 核实磁盘或数据库结果；不返回“正在安装”的202，也没有任务状态查询。

新增路由的 Allow 分别为：`/pishoo/proxies` 使用 GET、PUT、PATCH、DELETE；`/pishoo/libs` 使用 GET；`/pishoo/libs/{id}` 使用 GET、PUT、DELETE；`/pishoo/lib-check` 使用 POST。未知 query 参数返回400；仅代理 GET/DELETE 允许本文定义的 location 参数。GET 和 DELETE 不接受非空 Body。`/pishoo/settings` 沿用现有设置 API 的方法与响应约定。

## 四 监听设置

### 查看设置

```sh
pishoo listen --id alice.smith
```

对应 `GET /pishoo/settings`，返回：

```json
{"listen":3}
```

CLI 展示保存值及其名称，例如 `both (3)`。GET 和本机查询读取数据库，不宣称这是正在运行的监听范围。

### 修改设置

```sh
pishoo listen --id alice.smith off
pishoo listen --id alice.smith internal
pishoo listen --id alice.smith external
pishoo listen --id alice.smith both
```

同时接受数字 `0`、`1`、`2`、`3`，分别对应上述四个名称。CLI 先解析成现有 `u8` 值，无效输入在开始写操作前拒绝。

对应 `PATCH /pishoo/settings`：

```json
{"listen":1}
```

返回保存后的设置对象。复用现有 SQLite 即时事务、schema v1 和完整校验。不新增 settings 列，不恢复 exec；空 PATCH、未知字段、null 和越界值仍返回400。

保存后提示“Status:     Saved to disk. Restart Pishoo to apply changes.”。设置为0并重启后 HTTP API 不再可达，本机命令仍可恢复为1、2或3。命令不自动重启服务。

## 五 代理规则

### 查询规则

无参数、`list` 或 `ls` 对应 `GET /pishoo/proxies`，返回完整数组，按 location 排序。

```json
[
  {"location":"/test","proxy_pass":"http://127.0.0.1:8080"}
]
```

提供一个 location 对应 `GET /pishoo/proxies?location=...`，返回单个对象；未找到返回404。query 必须恰好出现一次 location，未知或重复参数返回400。保留无 query 的既有全表行为。

精确匹配的位置沿用 `= /test`：

```sh
pishoo proxy --id alice.smith '= /test'
```

对应 `GET /pishoo/proxies?location=%3D%20%2Ftest`。以 query 表达 location，避免把包含斜杠和空格的配置键放进路由参数。

### 添加与更新规则

```sh
pishoo proxy --id alice.smith /test 127.0.0.1:8080
pishoo proxy --id alice.smith /test http://127.0.0.1:8080/api/
```

对应新增 `PATCH /pishoo/proxies`，请求必须且只能包含两个字符串字段：

```json
{"location":"/test","proxy_pass":"http://127.0.0.1:8080"}
```

location 不存在时新增，存在时覆盖该条；返回保存后的单个对象。其他 location 不变。完整校验后进入 SQLite 即时事务，仅更新对应唯一键，提交前复核持久化配置。

沿用现有代理规则：上游只接受明确端口的回环 HTTP/TCP 地址；裸 `127.0.0.1:8080` 规范化为 HTTP URI。`127.0.0.1` 没有端口，返回400；不推定端口80，不扩展为外部地址或 DHTTP 上游。

`/test` 按路径段前缀匹配 `/test` 和 `/test/a`，不匹配 `/testing`；`= /test` 仅匹配该路径。未写上游路径时保留原请求路径；显式 `/` 或 `/api/` 替换匹配前缀。保留路径和重复规则沿用现有检查。

新增单条 PATCH 的目的，是让客户端直接表达更新某个唯一键。客户端不得为了这个操作自行 GET 全表再 PUT 全表，否则可能覆盖另一个客户端刚新增的规则。

### 删除规则

```sh
pishoo proxy --id alice.smith remove /test
pishoo proxy --id alice.smith rm '= /test'
```

对应 `DELETE /pishoo/proxies?location=...`。必须提供恰好一个 location，不带参数返回400，不能意外变成清空操作。事务内删除准确的唯一键；已不存在也返回204，不接受模式匹配或批量路径表达式。

### 清空与整表替换

```sh
pishoo proxy --id alice.smith clear
pishoo proxy --id alice.smith replace ./proxies.json
```

两者对应 `PUT /pishoo/proxies`，复用现有整表替换行为。clear 使用 `[]`；replace 从文件读取完整 JSON 数组，遵守64 KiB上限。全部输入验证成功后在一个事务内替换列表，返回保存后的完整数组。

PUT 表达明确的整表覆盖，不自动合并并发更新，不引入 revision、ETag 列或配置历史。并发单条修改通过事务串行提交；并发整表覆盖按提交顺序生效。

所有代理写操作均保存到 config.db，重启后才更新路由。

## 六 Lib 管理

### 磁盘资源与运行资源

每个组件固定存放在 `<身份目录>/lib/<LibId>/lib.wasm`。数据位于 `<身份目录>/db/<LibId>/`，二者职责独立。组件清单中的 title 和 version 是显示信息；资源身份由目录名 LibId 决定，不由文件 basename 或 title 推导。

磁盘 API `/pishoo/libs` 展示下次启动要加载的组件；已有 `/workspace-api/libs` 展示当前进程已加载的组件。两者不承诺一致。不添加“待生效”持久字段、Lib 摘要、版本登记表或新的运行状态容器，也不根据 title/version 相同就推断文件相同。

### 列表与详情

```sh
pishoo lib --id alice.smith
pishoo lib --id alice.smith list
pishoo lib --id alice.smith info note
```

分别对应 `GET /pishoo/libs` 和 `GET /pishoo/libs/note`。有效条目的字段沿用现有已加载目录的形状：

```json
{
  "id":"note",
  "title":"Note",
  "version":"1.0.0",
  "description":"个人便签",
  "endpoints":[{"method":"GET","path":"/api/note/","description":"打开便签"}]
}
```

集合响应为上述条目的数组，按 id 排序。元数据从当前磁盘文件读取并经现有 `validate_lib` 校验后派生；此查询不编译、不执行 guest，也不写入文件。endpoints 的构造规则与已有运行目录一致。

单个损坏组件不妨碍集合列出其他资源，集合中以互斥的错误条目表达：

```json
{"id":"note","error":"missing pishoo:openapi section"}
```

错误条目只包含 id、error，不同时携带正常 metadata。单条详情遇到无效组件返回400，缺失返回404；目录读取失败返回500。列表成功仅说明磁盘清单可读，不保证宿主编译成功，完整兼容性由 check/install 检查。

### 运行目录查询

```sh
pishoo lib --id alice.smith --loaded
```

这一命令明确使用同名身份加载客户端 Endpoint，调用该身份的 `GET /workspace-api/libs`。保留已有 owner 校验和响应数组。此操作读取运行进程，服务不可达时返回错误，不退回磁盘列表冒充运行结果。

为保证查询本机正在运行的目标，第一版命令仅在所选身份可连接时提供这项在线查询；HTTP API 的目标仍是实际建立连接的身份服务，不把连接成功等同于系统服务管理器确认某个 PID。输出明确标注“Catalog:    loaded (running service)”，磁盘列表标注“Catalog:    installed (disk)”。

### 安装与更新

```sh
pishoo lib --id alice.smith install note ./note.wasm
```

对应 `PUT /pishoo/libs/note`：

```http
Content-Type: application/wasm
Accept-Versions: v1
```

Body 为文件原始字节。安装和更新使用相同操作，允许用同一个 LibId 替换组件，不根据 OpenAPI version 自动拒绝降级。返回保存后的元数据对象。

操作顺序如下：

1. 校验 LibId、上传大小、目标目录类型和访问权限。未初始化的本机身份明确报错，不借安装命令创建身份凭据或初始化所有数据库。
2. 读取一次完整组件字节，先调用 `validate_lib`，再调用现有 `WasmRuntime::compile` 检查组件编译、imports 与 incoming-handler 接口兼容性。不实例化 guest，不调用 `Lib::load` 创建数据目录，不改变当前 Sandbox.libs。
3. 将同一份已校验字节写入同一文件系统的私有临时文件，完成写入与文件同步。新建目录使用0700，组件文件使用0600；已有目录权限保持不变。
4. 在文件修改锁下重新检查目标路径。首次安装把已完整准备的 `lib/<id>` 目录一次发布；更新通过同目录文件 rename 原子替换 `lib.wasm`。文件和目录同步完成后才报告保存成功。
5. 清理当前调用自己的临时资源，返回元数据并提示重启。运行中的 Router、Lib 和在途 guest 均不替换、不取消。

首次安装的临时目录放在身份目录中且位于 lib 扫描根之外；更新的临时文件放在目标组件目录中，不能使用另一个可能跨文件系统的 `/tmp` 来承诺原子 rename。不得在 lib 根下留下会被启动扫描当作 Lib 的临时目录。

校验或发布前写入失败，原组件保持不变。rename 已提交后如果目录同步失败，结果属于“提交结果需要核实”，不能声称原组件仍在；客户端通过 GET 或本机 info 检查，允许重试相同安装。不维护回滚版本或安装账本。

### 移除

```sh
pishoo lib --id alice.smith remove note
pishoo lib --id alice.smith rm note
```

对应 `DELETE /pishoo/libs/note`，无 Body。文件修改锁下检查目录，将 `lib/note` 原子移出扫描根，再清理本次调用的临时目录，成功返回204。目标不存在也返回204。

操作仅处理组件目录，保留 `db/note/`，不提供自动清库或 purge 参数。已有 daccess 规则也保持不变；安装和移除都不创建、删除或自动授权 API 规则。

移除在磁盘上提交后，当前进程的 note 仍可能被访问；重启后才撤销其路由。重新安装同一个 LibId 时继续使用保留的数据目录，其数据格式兼容由组件自身负责。

### 组件校验

```sh
pishoo lib check ./note.wasm
```

本机 check 不需要身份和配置库。与安装相同，执行 `validate_lib` 和当前宿主 `WasmRuntime::compile`，返回 title、version、description 和组件内声明的接口；不创建数据目录，不执行 guest，不检查业务逻辑或已有数据库兼容性。

对应 HTTP 能力为 `POST /pishoo/lib-check`，Body 和大小限制与安装相同，要求同名 owner 权限。成功返回200：

```json
{
  "title":"Note",
  "version":"1.0.0",
  "description":"个人便签",
  "endpoints":[{"method":"GET","path":"/","description":"打开便签"}]
}
```

check 尚未指定 LibId，接口路径因此是组件内路径，不构造 `/api/<id>`；失败返回400或基础设施故障500。远端 check 检查的是服务端宿主版本，本机 check 检查的是当前二进制版本。

### 并发与文件完整性

文件修改使用操作系统建议锁，锁附着在已打开的既有 lib 根目录资源上。安装和移除提交阶段取得排他锁；磁盘查询和启动扫描取得共享锁。打开句柄和锁只存在于当前调用局部，结束即释放，不新增持久锁表、锁成员、自定义 Guard 或后台任务。

HTTP handler 与本机命令使用同一锁规则，同一 Lib 的安装移除按提交顺序生效。编译在提交锁外完成，提交前重新验证目标；编译结果不进入构建队列。根目录锁只保护文件布局，不限制 Lib 执行并发。非 Pishoo 进程直接写文件不在建议锁的保障内。

文件操作使用目录句柄约束相对路径，拒绝组件目录、组件文件和身份布局中的符号链接，不能只检查路径后再通过可被替换的绝对路径写入。移除操作的目录清理不跟随目录内的符号链接。平台锁和相对目录操作须在 Linux/macOS 验收；复用现成文件与目录能力，不改 dhttp 或 h3x。

请求体尚未完整收取时上传断开，不进入安装提交。完整输入交给本机编译和文件提交后，即使响应端断开，该操作仍可能完成；不新增取消通知来承诺回滚，应按提交结果未确认处理并查询核实。阻塞的编译、SQLite 和文件操作复用现有 spawn_blocking 模式，进程内异步 I/O 不被同步等待长期占用。

服务启动扫描保持现有完整扫描与串行编译，任一组件失败仍使本次启动失败。本设计不恢复热更新、后台安装、编译协调器或旧版本回退。

## 七 服务管理

服务管理命令调用系统服务管理器，目标是已安装的 Pishoo 服务。不能为启动和重启虚构一个必须依赖服务存活的 HTTP 接口，也不让管理请求触发远端进程退出。

| 操作 | Linux 对应接口 | macOS 对应接口 | 成功含义 |
| --- | --- | --- | --- |
| start | `systemctl start pishoo.service` | `brew services start pishoo` | 管理器接受并完成相应操作 |
| stop | `systemctl stop pishoo.service` | `brew services stop pishoo` | 管理器确认服务停止 |
| restart | `systemctl restart pishoo.service` | `brew services restart pishoo` | 管理器完成停止与重新启动 |
| status | 系统服务管理器状态查询 | Homebrew 服务状态查询 | 返回管理器观察到的服务状态 |

命令使用明确程序名和参数数组启动本机管理器，不通过 shell 拼接命令，不调用 guest exec，也不新增 Pishoo 宿主执行 API。

Linux 管理现有 system service；macOS 管理当前用户的 Homebrew 服务。权限不足时返回管理器错误，不自动 sudo，不修改服务 User/DHTTP_HOME，不创建另一份用户服务。平台不支持或服务未安装时明确报错，不自动安装和注册服务。

状态查询展示管理器观察到的运行、停止或失败状态，不把“命令已受理”当作所有身份已初始化、Lib 已加载或 HTTP 已就绪。无需进程实例锁、PIDFile、健康标志或自定义服务状态容器。stop/start/restart 不接受 `--id`、不承诺单 Server 生命周期管理。

重启影响该服务加载的全部身份。资源修改命令只保存，不自动重启。Linux 服务覆盖文件中指定的 User/DHTTP_HOME 和 macOS 服务运行用户决定实际加载的目录；本机 CLI 操作的目录必须与部署匹配。

## 八 输出与完整操作示例

查询结果写 stdout；身份、目录、保存提示和诊断写 stderr。HTTP 的200/204与命令成功都不代表变更已经作用于运行进程。磁盘查询不额外推断是否需要重启。

命令退出约定：成功返回0；输入或用法错误返回2；存储、权限、连接、管理器调用和编译基础设施故障返回1。HTTP 错误由在线命令转换为失败输出；身份选择错误不继续到其他身份。

```sh
# 所选 Server 的磁盘配置与组件
pishoo listen --id alice.smith
pishoo proxy --id alice.smith
pishoo lib --id alice.smith

# 配置一个本机上游
pishoo proxy --id alice.smith /test 127.0.0.1:8080

# 检查并安装组件
pishoo lib check ./note.wasm
pishoo lib --id alice.smith install note ./note.wasm

# 保存完成后重启整个服务
pishoo restart

# 查询运行目录，验证该身份已加载 note
pishoo lib --id alice.smith --loaded

# 移除组件并重启，保留 note 数据
pishoo lib --id alice.smith remove note
pishoo restart
```

HTTP 客户端顺序对应：PATCH `/pishoo/proxies` → POST `/pishoo/lib-check` → PUT `/pishoo/libs/note` → 在目标机器上执行服务重启 → GET `/workspace-api/libs`。HTTP 客户端不拥有本机管理器调用权限时，保存后由目标机器上的运维操作完成重启。

## 九 冻结契约变更提案

以下具体变更由用户于2026-10-08要求按本文实施，已纳入冻结清单。现有 Server、Sandbox、Lib、WasmRuntime、Invocation 和 Error 的结构成员及方法签名保持不变；dhttp、h3x、数据库 schema 和运行生命周期保持现状。

### 管理命名空间迁移

配置路由、示例客户端、配置 API 文档及相关测试统一切换到 `/pishoo`。本设计不为旧 `/sys` 路径增加兼容别名或重定向；发布新版本时客户端须一同更新。现有 `/.pishoo/dhttp/` 正向代理路径属于已经冻结的独立接口，不随本次配置管理前缀调整。

`/pishoo` 根路径及其子路径成为管理保留命名空间，代理和 Lib 声明不能占用；`/pishoo-extra` 不受该路径段前缀影响。实施前检查既有代理配置和 Lib 清单是否占用 `/pishoo`；发现冲突直接报告，保留原始数据，不自动删除或改写规则。旧 `/sys` 前缀从管理保留路径列表移除。

### 路由行为

| 现有接缝 | 已批准变更 |
| --- | --- |
| `setup::config_router(profile, endpoint)` | 保持签名；配置路由迁入 `/pishoo/settings` 与 `/pishoo/proxies`，增加代理 GET 的 location 查询、PATCH 单条更新和 DELETE 单条删除 |
| `routes::reserved(path)` | 管理保留前缀由 `/sys` 改为 `/pishoo`，检查根路径及路径段前缀；新增 Lib 管理路由均在该命名空间内，保持函数签名 |
| `Server.load` 与 OCSP 刷新后的 Router 装配 | 挂载 Lib 管理 Router，捕获已有 profile、Endpoint、Sandbox.runtime 的克隆；不增加 Server 成员 |
| `Sandbox::load_libs` | 方法体增加局部共享文件锁，保持签名、串行加载、失败处理及启动规则 |
| `main` | 无参数沿用运行入口；带命令调用新命令入口，管理命令不进入 Server 启动循环 |

### 最小跨模块函数清单

以下采用现有 Pishoo Result、HTTP、JSON、OpenAPI 和资源类型，不新增命令状态 enum、配置 DTO、Lib 管理结构或状态成员。实现时以这组具体签名为审批范围，若发现仍有冲突须再提交差异。

```rust
// 库根重导出给同包二进制；实现放在普通 cli.rs。
pub async fn run_command(args: Vec<std::ffi::OsString>) -> Result<()>;

// setup.rs 中既有 module-local config_database 的签名和可见性变更。
// method/uri 表达上表的配置资源操作，CLI 已在本机完成身份目录选择。
pub(crate) fn config_database(
    profile: &dhttp_home::identity::IdentityProfile,
    method: &http::Method,
    uri: &http::Uri,
    payload: Option<serde_json::Value>,
) -> Result<serde_json::Value>;

// sandbox 逻辑模块的 Lib 管理接入。
pub(crate) fn lib_management_router(
    profile: dhttp_home::identity::IdentityProfile,
    endpoint: dhttp::Endpoint,
    runtime: std::sync::Arc<WasmRuntime>,
) -> axum::Router;

pub(crate) fn installed_libs(
    profile: &dhttp_home::identity::IdentityProfile,
    id: Option<&str>,
) -> Result<serde_json::Value>;

pub(crate) fn check_lib(
    bytes: &[u8],
    runtime: &WasmRuntime,
) -> Result<oas3::OpenApiV3Spec>;

pub(crate) fn install_lib(
    profile: &dhttp_home::identity::IdentityProfile,
    id: &str,
    bytes: &[u8],
    runtime: &WasmRuntime,
) -> Result<serde_json::Value>;

pub(crate) fn remove_lib(
    profile: &dhttp_home::identity::IdentityProfile,
    id: &str,
) -> Result<()>;
```

`config_database` 复用现有校验和事务实现，DELETE 的内部结果为 JSON null，由 HTTP 边界转换成204；本机命令不输出 null。Lib 函数在 sandbox 职责范围内实现，Router 与 CLI 都调用它们。既有公开 `validate_lib` 的签名和清单规则不变，`check_lib` 补上已有 runtime.compile 的宿主兼容性检查。

在线已加载目录查询在 CLI 内直接复用现有 Endpoint 请求能力，不新增 OutboundTransport 或 Endpoint 方法。文件锁与目录操作使用现成资源类型和模块内无状态算法，不创建 XxxGuard。

命令解析使用局部变量及标准/第三方解析结果，不引入自有持久参数容器；默认身份 TOML 解析可能需要一个解析依赖，具体依赖选型不扩展自有冻结结构。命令入口的错误退出分类在二进制边界处理，不新增 Error 变体。

gmutils 的 `identity ensite/dissite` 仍使用旧 server.conf 和 reload 提示，与当前 Pishoo 不兼容。其移除或改为明确迁移提示应作为 gmutils 的独立改动，不在本次变更中恢复旧配置，也不让两个仓库分别实现新的 Pishoo 配置写入。

## 十 实施顺序与验收

本次实施先更新批准的冻结接口，再实现配置存储复用和单条代理 API；随后实现 Lib 文件管理、对应 API 与命令；最后接入服务管理器并更新帮助和操作文档。始终保持无参数 `pishoo` 与现有安装服务文件兼容。

| 验收场景 | 预期结果 |
| --- | --- |
| `--id`、`-i` 以及前后参数位置 | 选择同一规范化身份，进程级命令拒绝身份参数 |
| 默认身份有效、缺失、无效或文件损坏 | 按规定选择或立即失败，不修改其他身份 |
| 服务停止或 listen=0 | 本机配置、安装和移除仍可使用；在线运行目录查询明确失败 |
| HTTP ACL 放行但身份不符 | 所有新增管理 API 仍拒绝 |
| 管理路径迁移和保留前缀 | `/pishoo` 路由可用；旧 `/sys` 无管理别名；配置冲突被报告；`/pishoo-extra` 可作为普通路径 |
| 代理 PATCH 与另一个 location 的 PATCH 并发 | 两条修改均保留，不以整表覆盖实现 |
| 重复删除和无参数 DELETE | 前者204；后者400，不清空全表 |
| URI 未写路径与显式 / 路径 | 保存、读取与重启后的转发语义保持区别 |
| 无效输入与数据库写入失败 | 不留部分规则；保持现有错误响应兼容 |
| 缺少 manifest、错误 component 或宿主 imports 不兼容 | check/install 失败，旧组件与数据不变 |
| 超大 Body、分块上传超过限制、中断上传 | 未收齐输入不提交；提交阶段断开需查询核实，始终不发布半个组件 |
| 首次安装、并发更新、更新与移除竞争 | 扫描根只出现完整资源，提交按文件锁串行 |
| 进程在临时写入和 rename 前后中断 | 启动不扫描临时目录，读取到完整旧文件或完整新文件；提交后同步失败如实报告 |
| 文件或目录为符号链接，修改期间替换路径 | 拒绝越界操作，不修改目标身份以外资源 |
| 列表包含损坏 Lib | 返回对应错误条目，正常条目仍可见；完整安装检查仍会编译 |
| 安装、移除后未重启 | 磁盘目录与运行目录可不同，当前请求和数据不受磁盘操作取消 |
| 重启后查询与实际 WASM 请求 | 新组件已加载或被移除；保留的数据仍能使用 |
| 服务管理器权限不足、未安装、不支持的平台 | 清晰失败，不自动提权或创建第二份服务 |
| Linux/macOS 真实部署 | 文件锁、原子发布、正确用户和目录、启动停止重启状态查询均验收 |

运行适合上述存储、并发与真实组件行为的测试；文档和帮助文本通过命令/API 映射检查。资源管理不会自动授予 Lib API 权限，授权集成仍以当前 daccess 库为依据。

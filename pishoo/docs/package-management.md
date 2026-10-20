# 软件包管理审查稿

日期：2026-10-20。基线：基于 `cf5bfbe` 的本次包管理修订；不是已发布版本或线上验收声明。

本文整合已确认的包管理约定，并逐项核对当前实现。**“当前行为”描述代码；“审查事项”记录缺口和待决定的保证，不代表已经批准新增功能。** 最新修订同步实施用户确认的商店安装统一 PUT、取消本地安装、删除本地防篡改字段及作者目录；没有部署、操作日常身份数据库或修改应用商店。

## 1. 范围与阅读顺序

| 文档 | 唯一职责 |
| --- | --- |
| **本审查稿** | 宿主 API、安装登记、生命周期、文件分布、一致性边界及待审查问题 |
| [发布包规范](package-spec.md) | WEBC v3、manifest、内容映射、签名、OpenAPI 与源包限额 |
| [genmeta 命令](genmeta-package.md) | CLI 语法、身份选择、构建发布、安装更新及当前客户端限制 |
| [packages.sql](../src/package/packages.sql) | 数据库精确 DDL；文中字段说明不能替代实际约束 |

建议先看[第 12 节审查事项](#12-审查事项)，再核对 [API](#4-http-api)、[数据库](#5-数据库)、[生命周期](#6-安装更新与卸载)、[文件布局](#7-文件分布与访问路径)。发布源格式和 CLI 操作在各自文档维护，不再复制多个互相覆盖的完整规范。

## 2. 身份、名称与职责

| 概念 | 例子 | 用途 |
| --- | --- | --- |
| 作者身份 | `alice.dhttp.net` | 源包签名及发布归属；数据库列名为 `author`，源 manifest/HTTP 字段沿用 `publisher` |
| 名称 | `calendar` | 同一宿主身份、同一类型内唯一的短名 |
| 完整包名称 | `alice.dhttp.net/calendar` | 由作者与名称临时组合；用于签名、依赖、HTTP 兼容字段和运行资源键，不是数据库 `id` 列 |
| 包类型 | `app` 或 `lib` | 决定源包内容、登记表与管理路径 |
| 包版本 | `1.2.3` | 固定的三段数值版本；具体语法见发布包规范 |
| 目标身份 | `sky.lee.dhttp.net` | 哪个 Pishoo 身份安装和运行该包；管理请求必须证明对此身份的控制权 |
| 系统账户 | 身份所在个人 home 的账户 | 决定磁盘 UID/GID；不从作者或 HTTP 调用进程推导 |

包身份按 `(类型, 作者, 名称)` 区分；一个发布版本再加 `version`。本机安装唯一性按 `(类型, 名称)` 区分：可以同时安装 `calendar` App 和 `calendar` Lib，同类 `alice/calendar` 与 `bob/calendar` 不可共存；换作者需显式卸载。名称本身不赋予标准服务权限。

CLI 可以省略约定的 `.dhttp.net` 后缀，例如 `alice/calendar`；内部签名和公开安装登记使用完整规范化身份。不要把数据库作者当作可任意填写的作者展示姓名。`--id` 在管理操作中选择目标身份，在 build/publish 中约束签名作者，详见 CLI 参考。

Pishoo 负责验证输入、运行资源和生命周期；gmutils 提供 CLI；Workspace 提供界面；独立商店负责发布记录和下载源包。宿主 DEB/RPM/Homebrew 包不携带业务 WEBC，也不在启动时下载默认业务包。身份配置、加载且服务可达后，才由其账户显式执行 `genmeta install workspace`。

## 3. 三层数据及其权威

本文“源包”指作者提供的**原始已签 WEBC 发布包**，不是 Rust/前端源码工程，也不是安装后的目录或临时运行快照。

```mermaid
flowchart LR
    Source[已签名 WEBC 源包] --> Validate[实际安装或更新时验证]
    Validate --> Registry[config.db 安装登记]
    Validate --> Files[WASM 与前端文件]
    Registry --> Runtime[临时无签名运行快照]
    Files --> Runtime
    Runtime --> Service[Lib 执行与 App 静态访问]
```

| 数据层 | 保存什么 | 不应据此推断什么 |
| --- | --- | --- |
| 发布源包 | 已签 manifest、内容、签名证书及版本说明 | 本机已经安装或运行 |
| 安装登记与文件 | 挑选后的元数据、关系及实际运行内容 | 能恢复原始已签 WEBC；登记存在即健康 |
| 运行快照 | 当前进程实际持有的编译组件或静态字节 | 它仍是可发布的原始签名包；与外部改写后的登记永远一致 |

安装、更新时检查商店下载的完整源包；安装后不保存包摘要、作者密钥指纹或证明材料，也不对本地文件进行防篡改复核。运行快照中的容器摘要仅用于组装原生 WEBC 格式，不与已保存的基准比对。

源 manifest 仍要求 `changelog` 和 Lib `host` 用途，供发布、输入验证和安装时查看；按本轮决定不复制到安装表。已安装 GET 不返回这些信息，也不通过额外的 check/openapi/loaded 接口补回。

## 4. HTTP API

### 4.1 路由和方法

App 与 Lib 对称。以下 `{type}` 只能替换成字面量 `app` 或 `lib`：

| 方法与路径 | 请求 | 成功结果 |
| --- | --- | --- |
| `GET /std/sys/{type}/list` | 无 query、无 body | 200，数据库已安装数组 |
| `GET /std/sys/{type}?name=NAME` | 一个短名、无 body | 200，数据库详情及 `Package-Id` |
| `PUT /std/sys/{type}?name=NAME` | 精确商店作者、版本 JSON | 200，安装或替换结果 |
| `DELETE /std/sys/{type}?name=NAME` | 无 body；可带删除身份前置条件 | 204，无响应体 |

安装与更新统一使用 PUT，声明该名称应安装指定作者的精确版本；不存在独立 `/update`。相同请求多次执行的预期安装状态与执行一次相同；这不保证只下载或编译一次，也不要求响应相同。CLI 的 install/update 区别来自前置读取及检查。POST 返回 405；基础路径的 Allow 为 GET, PUT, DELETE。参见 [RFC 9110 §9.2.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.2.2) 与 [§9.3.4](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.3.4)。

没有 check、openapi、loaded、后台安装任务、进度或包状态查询接口。旧 `/std/sys/package/*` 不提供别名。这里的 GET 只反映数据库登记。GET `/list` 当前不分页、不筛选，不接受 `?name=...`；详情按单个 name 查询。

名称必须符合源包短名规则。参数只允许一个 `name`，重复、未知参数、含作者的路径、解码后包含 `/` 均拒绝。参数值按百分号解码 UTF-8；不能把 `+` 当成空格。PUT 的 name 只放在 query，路由决定 kind，请求 JSON 不得另传 name 或 kind。HEAD 目前返回 405。

### 4.2 身份、版本与通用头

这些 URL 由现有 DHTTP 接入提供；以下 HTTP 报文是协议示例，不表示普通 TCP HTTPS 客户端可直接连接。请求须先经过原生 daccess，再由包管理核对可信访问者的名称与 SubjectId 均属于目标 Endpoint。放行 ACL 不会替代 owner 核对；客户端不能用 header、JSON 或作者名字伪造访问身份。

| 请求头 | 约定 |
| --- | --- |
| `Accept-Versions` | 可省略；提供时，逗号分隔值或多个同名头中必须有精确 token `v1`，缺少该 token 为 505；不可解析为文本为 400 |
| `Content-Type` | PUT 为 `application/json`，基础类型比较忽略大小写并允许分号参数 |
| `Expected-Package-Id` | 仅 DELETE 且有 name 时允许；建议原样复制详情 GET 返回的 Package-Id；现代记录为作者/名称，旧无作者别名可能仅为短名；不保护版本或摘要 |
| `Trailer` | 不支持；出现该头即拒绝 |

包处理器返回 `Cache-Control: no-store`、`Supported-Versions: v1`。前置 daccess 拒绝、审批或握手错误可能在进入处理器前返回，因此不能声称所有传输/授权响应都具有这些头。`Accept` 当前没有强制 JSON 内容协商。

### 4.3 从商店安装或更新

```http
PUT /std/sys/app?name=workspace
Host: sky.lee.dhttp.net
Content-Type: application/json
Accept-Versions: v1

{"publisher":"std.dhttp.net","version":"1.2.3"}
```

名称、身份和版本均为示例，不保证商店存在该版本。JSON 必须恰好包含 publisher、version 两个字符串字段，不接收 kind、URL、作者指纹、版本范围或 `latest`。版本选择由 CLI/界面在发起本次请求前完成。宿主连接固定商店并核对固定名称和 owner hash，不跟随任意下载 URL 或重定向；递归取得精确 Lib 依赖。

### 4.4 安装来源限制

仅支持从固定商店安装和更新。没有本地 WEBC 上传入口，原始包字节 PUT 返回 415；CLI 不提供 `install --file`，Workspace 不提供本地安装。`build` 和 `publish` 仍可在本地构建、签名并发布制品；发布不等于安装。

同类已安装名称的作者必须与请求一致，跨作者重名返回冲突；不能借更新切换作者。商店按原发布协议提供不可覆盖的作者/名称/版本制品，宿主递归处理精确 Lib 依赖。

### 4.5 查询响应

```json
[
  {
    "id": "alice.dhttp.net/calendar",
    "name": "calendar",
    "kind": "app",
    "version": "1.2.3",
    "title": "日历",
    "description": "个人日历前端",
    "publisher": "alice.dhttp.net",
    "license": "MIT",
    "repository": "https://example.com/alice/calendar",
    "entry": "/",
    "dependencies": [
      {"publisher":"alice.dhttp.net","name":"calendar","version":"1.2.3"}
    ],
    "endpoints": []
  }
]
```

这是字段示例。实际 GET 规则如下：

- `/list` 为数组，按派生完整名称排序；无登记时为 `[]`。详情为单个对象，缺失返回 404。
- `id` 与 `publisher` 是兼容接口字段，分别由 author/name 与 author 推导，数据库没有 id 列。历史无作者短别名没有 publisher。
- `name/kind/version/title/description/dependencies/endpoints` 始终存在；title 缺失回退短名，description 可以是 null，清单及 App 的 endpoints 为空数组，不表示 Lib 没有业务 API。
- license、repository 非 NULL 时才返回。App 增加固定 `entry: "/"`。
- Lib 详情在完整包 ID、登记版本与已加载 Lib 匹配时，额外返回 `openapi` 和按实际挂载前缀映射的 `endpoints`，供 Workspace 展示完整 API。未加载或版本不匹配时不附加 OpenAPI，保留登记元数据与空 endpoints。此读取不执行 guest、不重新读磁盘制品或验签。
- 不返回 changelog、host 或运行健康信息；App 不返回 OpenAPI，也不声明业务能力。数据库不保存包校验和或作者密钥指纹。
- 详情成功额外返回 `Package-Id: alice.dhttp.net/calendar`。

安装成功的 200 来自已验证的本次输入，可以额外含有 changelog、host、OpenAPI 和 endpoints；商店复用结果也可能返回精简登记对象。当前成功写入响应并非统一的、固定丰富度的资源表示，客户端不得要求 GET 一定带回上述安装预览信息。

### 4.6 卸载与前置条件

```http
DELETE /std/sys/app?name=calendar
Host: sky.lee.dhttp.net
Expected-Package-Id: alice.dhttp.net/calendar
```

管理服务仍接受写入时，无该头且登记已不存在，DELETE 幂等返回 204；关闭期间可能返回 403。提供后，查询到的完整身份与期望不符或已经消失时返回 409。前置条件只约束作者/名称，不锁定版本或内容；不是 ETag/If-Match、版本 CAS 或跨 GET/DELETE 事务。仍被依赖的 Lib 返回 409 并列出依赖者。

### 4.7 限额、时限与错误

| 项目 | 当前值 |
| --- | --- |
| PUT JSON body | 不超过 4,096 字节；读取 body 上限 15 秒 |
| GET / DELETE body | 长度须为 0；读取仍有 120 秒上限 |
| SQLite 目录读取 busy timeout | 3 秒 |
| 商店单次取得源内容 | 独立 120 秒等待；完整包不超过 64 MiB |
| CLI 商店安装等待 | 15 分钟，与宿主 body 读取计时不是同一层 |

这些数值不是总安装 deadline。编译、递归依赖和提交没有被同一个 120 秒总计时器覆盖。请求中断或响应丢失不构成事务取消协议；没有自动重试或独立任务状态 API。

| HTTP 状态 | 当前典型含义 |
| --- | --- |
| 200 | 数据库读取或安装/更新完成并返回结果 |
| 204 | 卸载完成或无登记的幂等删除 |
| 400 | 输入格式、签名、类型、商店发布证明不符或 WASM 编译/链接不兼容；非法 body/query |
| 403 | 未获 daccess 允许、不是目标身份 owner，或写生命周期已经关闭 |
| 404 | 已安装详情不存在；部分商店不存在响应也映射为此状态 |
| 405 | 方法不支持；带当前路径的 Allow |
| 409 | 同名作者冲突、依赖/路由/代理冲突或删除身份前置条件不符 |
| 413 / 415 | body 过大 / 媒体类型不支持 |
| 505 | Accept-Versions 未包含 v1 |
| 502 / 504 | DHTTP 出站错误 / 对应等待超时 |
| 500 | 未转为输入错误的 SQLite、I/O、配置或任务等内部故障；也可能发生在存储提交之后 |

显式 405/413/415/505 等分支可只有状态；其他多数包处理器错误为纯文本，非统一 JSON error 对象。5xx 文本使用通用存储/编译失败说明，具体原因写日志。前置 daccess 的 202 是审批响应，尚未表示安装成功或创建了包管理后台任务。

实现依据：[包路由与响应](../src/package.rs)、[错误映射](../src/error.rs)、[前置授权](../src/routes/access.rs)、[商店客户端](../src/package/store.rs)。

## 5. 数据库

软件包登记保存在身份的 `db/config.db`。`libs` 与 `apps` 是两个独立名称空间，各自以短名 `name` 为主键，因此同一身份可以同时安装同名 App 与 Lib，同类不能安装两个同名制品。`author` 保存规范化发布者身份；对外仍使用完整的 `author/name` 包标识，但不重复持久化 `id`。完整包标识用于运行时集合键；新 WASM 私有数据路径只使用短名。

来源：[`packages.sql`](../src/package/packages.sql)；`files.rs::package_identity`、`registered_packages`、`check_name`、`save_registration`。

两张登记表均为以下 7 列：

| 列 | SQLite 声明 | SQL 可空性/约束 | 正常公开安装路径的含义与应用不变量 |
| --- | --- | --- | --- |
| `name` | `TEXT PRIMARY KEY NOT NULL` | 非空且同表唯一；无名称格式 `CHECK` | 来自已验签包的模块名。应用检查长度 1–63，首字符为小写字母，其余为小写字母、数字或 `-`。`std` 是两种类型的安装保留短名。 |
| `author` | `TEXT` | 可空；无格式 `CHECK` | 公开 HTTP/商店写入使用已验签、规范化的完整发布者身份。旧短标识及冻结内部 helper 的短别名兼容路径允许 NULL；该情况下外部键仍为短名，不编造发布者。公开新安装不能据此绕过签名身份。 |
| `version` | `TEXT NOT NULL` | 非空；无版本格式 `CHECK` | 源包要求 `MAJOR.MINOR.PATCH` 精确版本。依赖也保存精确版本，不保存范围。SQL 类型本身不能保证版本合法或与依赖相符。 |
| `title` | `TEXT` | 可空 | 正常安装从已验签包写入非空展示名称；旧登记可无该字段，目录接口回退为短名。 |
| `description` | `TEXT` | 可空 | 正常安装从已验签包写入说明；旧目录输出可为 NULL。新布局运行快照需要该说明，缺失会报登记错误。 |
| `license` | `TEXT` | 可空 | 正常安装写入源包许可证声明；旧目录可缺失。新布局运行快照要求存在，不自动杜撰许可证。 |
| `repository` | `TEXT` | 可空 | 可选的源码仓库地址。源包给出时，安装检查 HTTPS、主机和无用户凭据等约束；源包省略时，即使是新安装也可以为 NULL。 |

在这两张核心登记表中，SQL 只直接强制 `name` 的非空、唯一以及 `version` 的非空。其余校验归 `format.rs::read_package/package_metadata` 与正常写入函数，不应将应用层不变量描述成数据库 `CHECK`。手工 SQL 可以写出正常 API 不会产生的记录。

不持久化 `id`、`manifest_json`、`changelog`、宿主用途描述、原始签名封装、`package_proof`、`package_sha256` 或 `author_owner_hash`。`kind` 由所属表确定；App 的 `entry` 固定为 `/`；`publisher` 在接口中从 `author/name` 推导。不存在只为软件包分配的独立 UUID、安装账本或后台状态表。

### 5.1 依赖与能力关系

| 表 | 主键及关键字段 | 外键与删除行为 |
| --- | --- | --- |
| `app_dependencies` | PK (`app_name`,`lib_name`)，另有非空 `version` | `app_name -> apps(name)` 删除级联；`lib_name -> libs(name)` 删除限制。 |
| `lib_dependencies` | PK (`lib_name`,`dependency_name`)，另有非空 `version` | 来源 `lib_name -> libs(name)` 删除级联；目标 `dependency_name -> libs(name)` 删除限制。 |
| `lib_capabilities` | PK (`lib_name`,`capability_id`)，非空 `title` | `lib_name -> libs(name)` 删除级联；`capability_id` 非空字符串；`trim(title)` 非空。 |
| `lib_capability_operations` | PK (`lib_name`,`capability_id`,`method`,`path`)；UNIQUE (`method`,`path`) | (`lib_name`,`capability_id`) 指向能力表并删除级联；方法须非空且为大写，路径须为非根绝对路径，保存按已验签发布者映射后的实际路径。 |

依赖只指向 Lib；安装 App 不会令 App 成为另一制品的依赖。关系键保存同类唯一短名，查询时连接 `libs/apps` 还原作者与完整标识。依赖要求精确到作者、名称、版本，例如 alice.dhttp.net/calendar@1.2.3，不能以 bob.dhttp.net/calendar@1.2.3 替代；作者不必重复保存在关系行中。`version` 并非外键的一部分，SQLite 不会自动检查它与被依赖 Lib 的登记版本一致。

`files.rs::save_registration` 在同一即时事务内检查精确作者、短名与版本，替换来源制品的出边；Lib 更新还检查所有入边要求的版本。`check_dependency_cycle` 对正常 Lib 写入检查环。安装结束前进行 `pragma_foreign_key_check`。这些检查不在启动时重复执行。

已有入边的 Lib 必须维持依赖要求的作者、短名和版本。安装后不保存或比较源包内容摘要；相同版本的安装输入仍经过正常验签和兼容校验，不以本地旧文件作为防篡改基准。商店已发布版本不可覆盖由商店协议保证。

卸载 App 只删除其登记、静态制品和自身依赖出边，保留依赖 Lib。卸载一个仍有 App/Lib 入边的 Lib 会被拒绝并列出依赖者。卸载父 Lib 删除其出边和自身能力登记，不自动卸载因此不再被引用的依赖；业务数据库与既有 ACL 均保留。

能力表只登记源 OpenAPI 的业务能力组及其操作；2026-10-20 目标字段改为文档级 `x-access-groups`，操作级 `x-access-group` 和 `x-initial-access`，详见[能力接口分组](package-spec.md#能力接口分组)。当前解析器仍读取旧 `x-pishoo` 嵌套，本次规范修订不代表运行时已切换。它不是全部 HTTP 路由、宿主 imports、ACL/Contact/包管理服务的统一登记表。SQL 的全局方法/路径唯一约束只覆盖已登记的能力操作；不同 Lib 不得拆分同一路径的方法等更强规则，由安装时 `check_routes` 检查。

来源：`packages.sql`；`files.rs::capability_rows/save_registration/check_dependency_cycle/remove_artifact/check_routes`；原生能力查询见 `server.rs::native_router` 和 `contact.rs` 中的能力目录查询。

## 6. 安装、更新与卸载

### 6.1 单制品磁盘与数据库写入

`files.rs::install_artifact` 的实际主要顺序如下：

1. 取得已初始化身份的真实目录句柄及身份级排他建议锁；定位 `libs` 或 `apps`。必要时，先在独立事务中升级旧软件包登记 schema。
2. 校验原始签名输入、类型、完整身份、宿主兼容与 WASM 可编译性；核对同类短名、作者及精确依赖版本。
3. 在身份目录下的随机 `.pishoo-*` 临时目录准备新实际载荷，并设置归属、同步文件。这里没有运行 guest。
4. 取得相应制品根目录的排他锁，复核身份/根目录句柄未被替换，检查目标与身份位于同一文件系统。
5. 开始 SQLite `IMMEDIATE` 事务；检查代理、路由和已有 ACL 占用；写登记、依赖及能力。
6. 将旧目标移到临时目录内的 `.previous`；把新 WASM 文件或整个 App 目录重命名为正式目标；同步目标父目录和身份目录。
7. 提交 SQLite；解除回滚守卫；清理旧目标及临时目录。清理旧 App 备份的失败被忽略，不等于重新撤销已经提交的安装。

身份级锁串行化合作的安装/卸载；根目录锁使读取载荷快照的合作读取者不会观察到发布的中间状态。它们是建议锁，不约束直接用其他程序修改 SQLite 或文件的管理员。

### 6.2 在线内存发布

商店流程取得 Sandbox 写锁并确认服务未关闭、所需依赖仍匹配后，调用单制品存储事务；该事务完成输入校验和 WASM 编译。提交成功后读取实际运行快照，装配 Lib 并准备私有数据目录，最后替换 `Sandbox.libs` 中的 Arc。失败的名称、依赖、路由或 ACL 检查不会先创建新短名业务目录，从而避免遮住保留的旧作者目录。App 在存储提交后读取实际静态快照并替换 `Sandbox.apps`。

已有请求持有的旧 Arc/字节快照仍可完成；正常更新或卸载不会强行把已开始的 guest 替换为新版本。内部存储 helper 供商店流程及测试使用，不构成公开的本地安装入口。

商店依赖安装按依赖顺序逐制品提交并发布；它不承诺整个依赖集合共用一个 SQLite/文件事务。后续父制品失败时，先前成功的依赖及其数据保留。

宿主 upsert 没有统一的“版本必须递增”约束：受依赖 Lib 的作者及版本必须保持；未被依赖的 Lib 和 App 可以选择商店中其他精确版本。商店已发布版本不可覆盖，宿主不另存内容摘要。

### 6.3 卸载

`remove_artifact` 在身份排他锁下确认登记，拒绝删除仍有入边的 Lib；在根目录排他锁与即时事务内删除登记并将实际制品移动到临时目录；同步后提交，再清理临时目录。在线入口随后从 Sandbox 集合撤销对应项。

没有登记的目标按幂等成功处理。已有登记但实际目标缺失时，当前卸载仍会报读取/定位错误，不能将它描述为“无条件清理损坏登记”。直接对已发布精确版本发起 PUT 可以重新写入实际载荷。 该重装路径要求管理身份仍能加载并可达；重启后若因载荷缺失而无法加载，在线修复不可用，见 R06。

来源：`files.rs::install_artifact/remove_artifact/check_lib_root/write_component`；`package.rs::lib_management_request`；`store.rs::install_store_artifact`。

## 7. 文件分布与访问路径

以一个已加载身份目录 `<identity>` 为边界：

```text
<identity>/
├── db/
│   ├── config.db
│   ├── access.db                   # 原生授权；不随软件包卸载
│   └── <lib-name>/...               # 逐 Lib 私有数据，guest 看到 /db
├── libs/
│   ├── std/<lib-name>.wasm         # 验签发布者为 std.dhttp.net
│   └── <lib-name>.wasm             # 其他发布者
└── apps/<app-name>/public/...
```

标准目录由已验证发布者身份分类，熟悉的模块名不会令普通作者自动进入 `libs/std`。该目录没有额外运行特权。App 不区分 `apps/std`；新安装不会写 `manifest.json`、`signature.json`、OpenAPI 旁置文件或恢复原包的 proof。 此图只展示包相关资源；ssl、file、logs、repo、templates 和原生 contact.db 仍按原生基础设施布局保留，不属于业务包卸载范围。

App 使用 `/<name>/` 及其子路径；标准发布者 `std.dhttp.net` 的 Lib 使用 `/std` + OpenAPI 声明路径，其他 Lib 使用 `/<name>` + 声明路径（普通 journal 声明 `/std/xxxx` 对外为 `/journal/std/xxxx`）。磁盘中的 `std` 分类不决定 App URL，也不会令 Lib 使用 `/<author>/<name>` 路由。配置、文件、DHTTP 等原生基础设施路径仍由各自服务定义。

App 安装检查已存代理，配置写入也检查已装 App，均在各自现有的即时事务内完成。App 的根和子路径不得被显式代理占用；祖先、邻接名称及未安装名称仍可代理。它不是全局保留 `/app` 或全部任意应用名。

`Lib::load` 才为 Lib 准备私有目录并将实际目录 preopen 为 `/db`。App 本身不获得单独的 WASI 数据目录。新路径为 `db/<name>/`，作者不作为文件或目录名。已有 `db/<author>/<name>/` 在新短名目录不存在时继续读取，避免静默丢失业务数据；不新建作者目录，不自动搬迁或删除旧数据。旧内部短别名沿用 `db/<alias>/`。

来源：`files.rs::resource_parent/write_expanded/check_routes/legacy_parent`；`runtime.rs::Lib::load`；`setup.rs::check_proxy_mounts`。

## 8. 输入校验与启动加载

发布与商店下载仍使用原始已签 WEBC。`read_package` 校验完整输入、签名和包规范；没有签名卷的输入在早期直接拒绝。公开安装从已验签包取得作者和短名，不接受客户端通过数据库字段或 URL 冒充作者。

安装只保存执行所需载荷和经过挑选的登记字段。现代 Lib 已有顶层 `pishoo:openapi` custom section 时，WASM 原样保存。被兼容接收的旧源包若只在 metadata 卷携带 OpenAPI，`runtime_wasm` 会把已验证的原始文档作为最后一个顶层 custom section 追加到安装 WASM；不保存追加前源包的 SHA256。

启动时 `disk_bytes` 的现代路径把数据库元数据、关系表和实际 `.wasm` 组合成一个没有签名卷的临时原生 `webc::Container`；App 使用实际 `public/` 目录生成类似内存快照。该容器保持现有运行时类型，不是原始源包，不能重新发布、不能冒充原始摘要，也不能通过 `read_package` 的源包验签入口。它不包含已丢弃的发行说明或宿主用途描述。

App 快照遍历拒绝符号链接、特殊文件和跨设备目录/文件；当前实现限制目录深度最多 64、目录项最多 10,000、文件总量最多 64 MiB。Lib 实际 WASM 读取也有 64 MiB 边界。这里的运行快照限制与源包输入大小限制是不同位置的检查。

启动按数据库登记选择制品，不扫描未登记文件补齐安装。它做必要的读取、解析、编译、链接及运行资源装配，但不重新验原始签名或比对本地文件摘要，也不重新审计全部依赖、路由或外键图。实际文件被修改后，可能读取/解析/编译失败，也可能在仍可执行时被加载；宿主不提供本地运行文件防篡改检测。

商店流程校验本次下载的原始发布包；复用已加载依赖只核对数据库作者、短名、精确版本及当前版本，没有本地载荷、摘要或作者密钥指纹比对。无签名快照不会送入源包验签入口。见 `store.rs::can_reuse_lib/install_store_artifact`。

已安装 GET 清单/详情从 SQLite 只读事务读取登记和依赖，不要求载荷存在、正在加载或可编译。Lib 详情仅在完整 ID 和版本匹配时补充已加载 Lib 的 OpenAPI 与实际 API 路径；清单及 App 详情不读取运行集合。查询不调用 guest，也不重新打开 WASM 或静态资源。数据库目录快照与当前运行集合可能暂时不同，不应将 GET 结果解释为运行健康检查。

来源：`files.rs::runtime_wasm/installed_bytes/runtime_snapshot/snapshot_directory/registered_metadata/installed_libs/installed_apps`；`sandbox.rs::Sandbox::load_libs`；`package.rs::installed_catalog`；`runtime.rs::Lib::load`；`format.rs::read_package/package_metadata`。

## 9. 回滚与异常中断

存储提交之前发生可处理错误时，未提交的 SQLite 事务回滚，作用域守卫尝试把新载荷撤回并恢复旧目标。应描述为可处理错误路径上的协调回滚。恢复 rename、同步及清理中的部分错误以 `let _ = ...` 忽略；若底层文件系统也拒绝恢复，不能保证文件最终必定回到原位。

SQLite 自身事务、文件重命名、运行时集合是三个提交边界。不存在跨它们的恢复日志或启动修复协调器，不能宣称断电/SIGKILL 原子性。下表依据当前提交顺序作静态推导；本轮未进行 SIGKILL、断电或文件系统故障注入实验，具体持久化结果还取决于底层文件系统与 SQLite 恢复结果。

| 中断位置 | 可能留下的状态 | 下一次行为 |
| --- | --- | --- |
| 只准备了 `.pishoo-*`，尚未发布 | 旧登记和旧目标，外加临时目录 | 启动忽略未登记临时内容；不会自动回收或导入这些制品。 |
| 旧目标已移走，新目标尚未就位，SQLite 尚未提交 | SQLite 恢复旧登记，正式文件缺失，旧文件可能留在临时目录 | 读取/启动通常报错；没有自动寻找 `.previous` 的恢复流程。 |
| 新目标已就位，SQLite 尚未提交 | SQLite 可恢复旧登记，但正式目标已经是新载荷 | **可能报错，也可能把新载荷和旧数据库元数据组合后成功加载。** 启动没有原始源摘要与实际文件的一致性核对，因此不能承诺一定检测出这个窗口。 |
| SQLite 已提交，尚未完成内存发布 | 新登记、新磁盘载荷；当前进程仍可能持有旧运行集合 | 重启通常按新登记装配；进程仍活着但后续快照读取失败时，API 可以返回错误而旧内存暂留。 |
| 卸载移动了文件但 SQLite 尚未提交 | 旧登记恢复，正式目标缺失，制品留在临时目录 | 可能启动失败；无自动撤销移动。 |
| 卸载已经提交但清理未完成 | 无登记，临时目录可能仍含旧内容 | 未登记内容不会自动重新出现。 |

文件 `sync_all`、目录同步及 SQLite `synchronous=FULL` 用于持久化各自操作，不能把这张表中的跨资源窗口合并为一个事务。

还应区分提交前错误与提交后错误：在线入口的 `disk_bytes/installed_apps` 重读发生在存储函数成功提交之后；这时发生错误不会自动撤销新登记或新磁盘。卸载函数也可能在提交后的最终目录同步返回错误，导致调用者尚未撤销旧内存项。这些情况不宜统一写成“请求失败则所有状态不变”。

## 10. 旧版本兼容

启动和只读目录兼容已发行的旧 `id/name/version` 表及其旧外键列，不通过扫描载荷生成新登记。进入底层安装或卸载 helper 的 `installation_database(..., true)` 路径时才触发自然键迁移。直接调用底层 `remove_lib/remove_app` 卸载不存在名称时，也先完成适用的升级；但公开 HTTP 适配层可以在发现名称未登记后直接返回，不进入该 helper，因此不能声称每一次幂等 DELETE 都必然触发迁移。初始化对新库使用 `packages.sql`，不会借启动读取自动导入历史制品。已采用 name/author 的中间库在显式管理写入时删除 package_sha256、author_owner_hash 两列；读取和启动不依赖它们。

迁移在一个即时事务中重建六张软件包表：把完整旧 `id` 分解为 `author/name`，把所有依赖、能力外键转换为短名；旧短标识保留 NULL 作者。未存在的展示字段保持 NULL，不通过文件扫描伪造这些事实。迁移检查复制行数和最终外键一致性，损坏引用或名称/身份不一致会中止并回滚；在该连接上临时关闭的外键执行检查会在事务结束后恢复。

兼容定位包括旧展开目录中的 `.webc-proof` 及其真实载荷，以及较早的 `lib/<publisher>/<name>/lib.webc`、`app/<publisher>/<name>/app.webc` 目录。旧 `.webc-proof` 只供读取既有布局，不会为新安装生成。显式重装写入新布局；不在启动自动搬迁、删除旧制品或业务数据。旧制品在重装后仍可能保留为未登记历史内容。

本次对话中未发行的“把 proof 存入数据库”中间 schema 不作为线上兼容格式。迁移检测到旧表 `package_proof` 有非 NULL 数据时明确报错并保留数据，不擅自删除或换名藏存。该防护不是最终 schema 的字段。

来源：`files.rs::installation_database/migrate_registration/package_identity/legacy_parent/artifact_bytes/expanded_bytes/restore_package`；`sandbox.rs::Sandbox::load_libs`。

## 11. 文件归属与隔离边界

Linux 系统服务仍可为 root 进程并加载多个账户身份。个人目录的所有者依据是所属 home 的 UID/GID，而不是 HTTP 调用者、软件作者或进程 root；全局系统身份沿用其目录归属。`server.rs::initialize_directories` 修正个人 `.dhttp` 和身份目录，再为原生基础目录做初始化及递归修复。

递归修复只处理真实目录和单链接常规文件，跳过符号链接、特殊文件、外部硬链接及不同设备的子树。递归身份/guest 修复通过句柄恢复已有模式位；这不是所有数据库归属代码路径的统一保证，见 R07。安装新载荷文件使用 0600、目录使用 0700，并在正式发布前继承身份的 UID/GID；不把已有用户目录统一 chmod 为新建模式。

`files.rs` 的制品读取、创建和重命名采用打开的目录句柄及 `openat`/`NOFOLLOW`。新展开不创建符号链接，读取快照拒绝链接和特殊项。已有现代 Lib 的目标常规文件若多链接，更新会拒绝；只读载荷读取本身并非“拒绝所有硬链接”，重点是避免通过归属修复或更新改变身份之外的 inode。

SQLite 主文件、WAL、SHM 以及在相关路径存在的 journal 也有归属处理；包存储的 `database_owner` 使用 no-follow 句柄并拒绝非单链接常规文件。普通读库与 SQLite 自身可能仍产生或保留附属文件，不能把“只有 config.db”写成目录布局完整保证。

Lib 数据目录在 preopen 前准备；guest 看到的 `/db` 只对应自己的实际目录。Pishoo 没有为每个调用切换进程 UID。guest 执行期间新建内容可能短暂使用 root UID；guest 任务退出或取消时，作用域清理按实际 preopen 句柄修复，而不是跟随可能变化的文本路径。清理失败只记录日志；SIGKILL/断电无法执行清理，下一次身份加载才重新修复。

来源：`server.rs::initialize_directories/repair_identity_ownership`；`files.rs::inherit_owner/database_owner/child_dir/expanded_file/write_component`；`runtime.rs::Lib::load/repair_guest_ownership/repair_guest_directory`；`setup.rs::inherit_config_files`。

## 12. 审查事项

以下问题来自代码核对；已按用户新决定取消的范围与仍待处理的问题分别说明。代码静态推导与已运行的测试分开记录。

| 编号 | 当前事实及证据 | 需要审查的决定 |
| --- | --- | --- |
| R01 | **本地安装已取消支持。** CLI --file、Workspace 本地安装和宿主原始 WEBC 上传入口均已删除。 | 不再通过修正媒体类型恢复本地安装；依赖统一由商店安装流程处理。 |
| R02 | **发布兼容尚未闭合。** 构建器生成内嵌 OpenAPI 的 Lib；未改动的 [store-server](../../../store-server/src/store.rs) 仍拒绝该嵌入并要求旁置文档。 | 是否把独立商店适配作为发布门槛；当前不能把“本地 build 成功”写成“可上传现网商店”。商店本轮保持不动；取消本地安装后也没有直接上传宿主的绕过路径。 |
| R03 | **跨数据库与文件系统没有崩溃原子性。** 存在旧登记与新载荷组合后仍可加载的窗口，第 9 节给出具体顺序。未做断电/SIGKILL 注入。 | 接受当前可处理错误下的协调回滚，还是另行定义进程中断后的恢复要求；不能仅在文案中称为完整原子事务。 |
| R04 | **提交成功后仍可能返回错误或保留旧内存。** 在线快照重读/最终同步在提交之后，错误不回滚已经提交的数据库和磁盘。 | 客户端如何确认结果，及是否需要明确的存储结果与运行生效结果约定。GET 当前不是健康检查。 |
| R05 | **部分带点深链接可能被当成资源缺失。** [App 分派](../src/sandbox.rs) 以末段含点判断静态资源；[contactPath](../../std/apps/workspace/src/lib/routes.ts) 会生成 /workspace/contacts/bob.dhttp.net，直接打开时按代码可推导 404，客户端内导航则可能正常。未新增浏览器复现。 | 明确静态资源与页面深链接的判定规则，并覆盖真实身份名深链接刷新。 |
| R06 | **修复入口与 CLI unchanged 语义不同。** 精确商店版本 PUT 可以重新写入文件，但 CLI install/update 遇到相同 ID/version 会返回 unchanged。登记存在但文件缺失的卸载也不保证成功。 | 仅在目标身份管理服务仍可达、精确版本已发布且商店可用时，可以直接 PUT 修复。载荷缺失可能令身份加载失败；当前不提供本地或离线修复入口。 |
| R07 | **归属与路径保证需限定阶段。** setup::inherit_config_files 的 fchown 没有统一恢复特殊模式位；Lib::load 初始数据目录 fchown 未按递归修复同样检查 dev；SQLite 仍有先检查路径再按路径打开的边界。 | 是否要求所有阶段同等的权限位、跨挂载和无竞态路径保证。当前 root 专用测试未在本轮实际 Linux root 环境执行。 |
| R08 | **协议与数据库约束仍有刻意简化。** 核心表允许多列 NULL、部分格式只由应用约束；错误多为文本；已安装 GET 不分页，而 CLI 清单单响应最多读取 2 MiB、等待 120 秒；响应头前置条件不保护版本；CLI 预检与写入不是同一事务。 | 逐项确认哪些是接受的简化，哪些必须在对外稳定前约束；不要把它们误写成已有的数据库或 CAS 保证。 |

业务数据另有明确边界：更新/卸载不迁移或删除 Lib 私有库；正常更新允许在途旧代码继续运行，新旧代码可能共享同一数据目录。业务数据库 schema 的前后兼容由 Lib 自身负责，宿主没有替业务实现数据回滚。新私有目录按短名定位，因此卸载后换作者安装同名 Lib 也会复用保留的 db/<name> 数据；宿主没有按作者另建隔离目录。

## 13. 验证证据与发布判断

| 关注点 | 当前证据 | 边界 |
| --- | --- | --- |
| 输入、归属、依赖、命名和路径冲突 | [包存储测试](../tests/unit/package/packages.rs)、[管理测试](../tests/unit/package/management.rs) | 主要为进程内或临时目录测试，不等于断电保证 |
| 旧表迁移、关系及失败回滚 | packages.rs 的迁移/失败用例，源 DDL | 不保证复制管理员自定义触发器/任意 schema 扩展 |
| 无签名运行资源与源包验签分离 | packages.rs、[商店客户端测试](../tests/unit/package/store.rs) | 不构成启动时的文件防篡改审计 |
| 页面安装、卸载和路径导航 | [Workspace E2E](../../std/apps/workspace/e2e) | 当前页面有 R05 的代码推导边界，既有通过数量不能证明所有身份名路径 |
| Linux root 归属、真实 HTTP/3 和公网商店链路 | [身份初始化](../tests/unit/server/initialization.rs)、[真实网络用例](../tests/unit/server/workspace_network.rs) | 部分测试按所需环境跳过；R02 尚未端到端闭环，R01 已按范围决定关闭 |

本轮验证：Rust 工作区 154 项通过、34 项按环境跳过，all-targets 编译检查通过；隔离商店真实 QUIC/JSON PUT 用例另行通过，覆盖重复 PUT 的登记/依赖一致性和路由冲突后的旧数据保留。gmutils 36 项单元测试、6 项离线打包测试及独立实际 PUT 协议用例通过；Workspace 27 项 Bun、11 项隔离 Chrome 桌面浏览器测试、源码类型检查和生产构建通过。文档链接、JSON 示例、精确 DDL 和四条路由引用校验通过。检查从中立目录执行 Cargo，避开上级目录混合本地/registry 传输库的配置；daccess 仍使用现有本地兼容实现。没有部署、操作日常身份数据库或修改商店代码。

审查时建议明确：支持的发布源/旧表范围、实际成功响应的承诺、异常中断恢复要求、更新与旧在途请求的数据兼容责任，以及 R02 是否必须在发布前关闭。本文没有替审查者预先给出“足够专业”或“可以上线”的结论。

### 实现追踪

| 主题 | 代码入口 |
| --- | --- |
| HTTP 请求、响应与内存发布 | [package.rs](../src/package.rs)：lib_management_router、lib_management_request、installed_catalog |
| 输入格式、清单和签名 | [format.rs](../src/package/format.rs)：read_package、package_metadata、verify_signature |
| 数据库、文件提交与兼容 | [files.rs](../src/package/files.rs)：installation_database、install_artifact、remove_artifact、runtime_snapshot |
| 商店取得源包与依赖排序 | [store.rs](../src/package/store.rs)：install_store_artifact、download_release、verify_release |
| 启动、目录归属与代理冲突 | [server.rs](../src/server.rs)、[runtime.rs](../src/sandbox/runtime.rs)、[setup.rs](../src/setup.rs) |
| CLI 预检、商店安装和身份选择 | 同级 gmutils 的 [install.rs](../../../gmutils/genmeta/src/install.rs)、[main.rs](../../../gmutils/genmeta/src/main.rs) |

跨仓路径按当前多仓工作区定位；它们是代码核对入口，不代表依赖仓库已经发布或部署了本稿中的新行为。

## 14. 精确数据库 DDL

以下为整理时从 [packages.sql](../src/package/packages.sql) 直接取出的内容；SQL 文件是精确实现源。后续修改应同时检查本稿与 SQL 是否仍一致。

```sql
CREATE TABLE IF NOT EXISTS libs (
    name TEXT PRIMARY KEY NOT NULL,
    author TEXT,
    version TEXT NOT NULL,
    title TEXT,
    description TEXT,
    license TEXT,
    repository TEXT
);
CREATE TABLE IF NOT EXISTS apps (
    name TEXT PRIMARY KEY NOT NULL,
    author TEXT,
    version TEXT NOT NULL,
    title TEXT,
    description TEXT,
    license TEXT,
    repository TEXT
);
CREATE TABLE IF NOT EXISTS app_dependencies (
    app_name TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
    lib_name TEXT NOT NULL REFERENCES libs(name) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    PRIMARY KEY(app_name, lib_name)
);
CREATE TABLE IF NOT EXISTS lib_dependencies (
    lib_name TEXT NOT NULL REFERENCES libs(name) ON DELETE CASCADE,
    dependency_name TEXT NOT NULL REFERENCES libs(name) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    PRIMARY KEY(lib_name, dependency_name)
);

CREATE TABLE IF NOT EXISTS lib_capabilities (
    lib_name TEXT NOT NULL REFERENCES libs(name) ON DELETE CASCADE,
    capability_id TEXT NOT NULL CHECK (length(capability_id) > 0),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    PRIMARY KEY (lib_name, capability_id)
);
CREATE TABLE IF NOT EXISTS lib_capability_operations (
    lib_name TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    method TEXT NOT NULL CHECK (length(method) > 0 AND method = upper(method)),
    path TEXT NOT NULL CHECK (substr(path, 1, 1) = '/' AND length(path) > 1),
    PRIMARY KEY (lib_name, capability_id, method, path),
    UNIQUE (method, path),
    FOREIGN KEY (lib_name, capability_id)
        REFERENCES lib_capabilities(lib_name, capability_id) ON DELETE CASCADE
);
```

包管理与运行时按用户确认拆为平级模块：`package.rs` 与 `package/` 拥有管理协议、下载校验和安装登记；`sandbox.rs` 与 `sandbox/` 拥有运行资源、执行和回收。在线管理通过既有共享 Sandbox 协调安装提交与运行引用替换，不增加管理状态或运行时包装。

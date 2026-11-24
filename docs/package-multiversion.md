# Pishoo 多版本包管理实施设计

日期为 2026 年 11 月 13 日。

本设计支持同一身份内同时运行同一业务包的多个精确版本。A 可以依赖 D 1.0.0，B 可以依赖 D 2.0.0。各版本独立加载代码，共用 D 的业务数据库、附件和其他持久资源。WASM 实体按精确版本直接保存到 lib，前端实体按精确版本直接保存到 app，活动前端由安装登记选定。

本设计涉及 Pishoo、Jade、Workspace 和业务包。用户已批准开始实施。数据库结构、执行上下文和 HTTP 选版字段按本设计及各仓库更新后的冻结清单落实。

## 1 设计约定

| 对象 | 约定 |
| --- | --- |
| 包身份 | 经过认证的 author 与完整 name |
| 软件版本 | manifest 中的精确 version |
| 安装单位 | 一个完整包版本及其精确依赖闭包 |
| 实体存储 | 身份目录内的 lib 文件和 app 版本目录 |
| 运行入口 | lib 下的精确版本 WASM 普通文件 |
| 前端入口 | 根据活动登记直接打开 app 下对应的版本目录 |
| 默认版本 | 每个包至多一个活动版本 |
| 依赖选版 | 当前执行包声明的精确依赖版本 |
| 外部选版 | 显式精确版本请求头或活动版本 |
| 数据归属 | 身份、标准或普通类别、完整包名 |
| 业务数据 | 同一包的各代码版本共用同一 volume |

完整包名中的每个 `/` 都形成真实目录层级。标准作者继续按宿主现行精确名单识别。数据库中保留真实作者、owner、原包摘要和签名证据。

2026-11-18 补充：运行时依赖允许形成循环，例如 Message 与 Group 互相声明精确版本。宿主按包身份与版本收集依赖闭包，已收集的版本不重复展开，已安装的匹配版本直接复用。安装在同一事务内先保存全部包身份，再校验并保存依赖边；启动无需拓扑排序。Lib 只记录依赖目标的完整身份与精确版本，调用时从 Sandbox 查找实例，避免强引用环。作者、签名、精确版本及卸载依赖保护继续生效。

## 2 Linux 动态库机制的对应关系

Linux 常见的动态库布局如下。中间的 `libd.so.1` 是 SONAME，末端是实际文件。动态加载器依据程序记录的依赖和搜索路径定位库。[ldconfig 手册](https://man7.org/linux/man-pages/man8/ldconfig.8.html) [ld.so 手册](https://man7.org/linux/man-pages/man8/ld.so.8.html)

```text
libd.so -> libd.so.1 -> libd.so.1.2.3
           libd.so.2 -> libd.so.2.0.1

A -> libd.so.1
B -> libd.so.2
```

Pishoo 采用版本文件名和按调用方解析依赖的方式。精确版本文件命名为 `message.wasm.0.2.2`。活动版本由安装登记保存，宿主根据选中的精确版本直接打开对应文件。清单中的依赖名称与版本分别保存在 `name`、`version` 字段。

WASM 文件由 Pishoo 和 Wasmtime 加载，包之间通过现有 WASI HTTP 接缝调用。宿主承担版本解析和实例选择。

当前依赖契约采用精确软件版本。ABI 别名属于后续兼容性规则扩展，届时需要单独定义 ABI 与软件版本的对应关系。

## 3 发布包布局

WAZ v2 的交付结构如下。

```text
manifest.toml
wasm/
└── std/
    └── comm/
        └── message.wasm
web/
├── index.html
└── assets/
docs/
└── manual.md
```

包内 WASM 使用完整包名。软件版本来自 manifest，安装时由宿主将原始 WASM 字节写入带版本的运行文件。

`manifest.toml` 继续采用 `format = "waz/2"`。最后的 `[signature]` 表、清单原始字节、交付文件、完整 ZIP 摘要，以及 WAZ v1 的历史验签规则按现行格式执行。安装后的运行路径和安装登记属于本地安装事实。[现行 WAZ 格式](../WAZ.md)

依赖声明示例如下。

```toml
[[dependencies]]
name = "std/comm/message"
version = "0.2.2"
```

Jade 的本地依赖继续支持 `path`。正式安装通过认证商店解析真实作者，历史依赖中的作者约束继续逐项验证。

## 4 身份文件布局

### 4.1 完整目录

下面以包含前后端的示例包 std/comm/message 展示两个正式版本、一个活动前端和一个共享业务 volume。实际包的前后端组成由清单决定。`ssl/`、`file/` 等现有身份资源按各自职责管理。

```text
<identity>/
├── ssl/
├── file/
├── lib/
│   └── std/
│       └── comm/
│           ├── message.wasm.0.2.2
│           ├── message.wasm.0.3.0
│           └── message.preview/
├── app/
│   └── comm/
│       ├── message.0.2.2/
│       │   ├── index.html
│       │   └── assets/
│       └── message.0.3.0/
│           ├── index.html
│           └── assets/
└── .volume/
    ├── std/
    │   └── comm/
    │       └── message/
    │           ├── database
    │           └── .file/
    └── sys/
        ├── setting/
        │   └── database
        └── pkg/
            ├── database
            └── std/
                └── comm/
                    ├── message-0.2.2.waz
                    └── message-0.3.0.waz
```

`message.preview/` 在预览期间存在。现有安装临时目录和恢复日志继续放在包管理控制的位置。

### 4.2 运行实体和活动前端

以 0.2.2 为活动版本时，宿主通过活动登记打开前端目录 `app/comm/message.0.2.2/`。各版本实体如下。

```text
lib/std/comm/message.wasm.0.2.2    普通 WASM 文件
lib/std/comm/message.wasm.0.3.0    普通 WASM 文件
app/comm/message.0.2.2/              普通前端目录
app/comm/message.0.3.0/              普通前端目录
```

前端 URL 沿用 `/app/comm/message/`。正式 WASM 文件和前端目录的内容对应原包交付内容，安装完成后按不可变版本管理。同一作者、包名和版本的另一个摘要视为内容冲突。

资源发现根据登记推导完整入口名称。现有按 `.wasm` 扩展名判断运行文件的扫描、预览、备份和清理逻辑，需要同步识别 `.wasm.<version>`。Jade 编译产物及 WAZ 内部文件继续使用 `.wasm` 后缀。

普通包使用 `lib/<完整包名>.wasm.<version>` 和 `.volume/<完整包名>/`。例如普通作者的 `tools/report` 使用 `lib/tools/report.wasm.1.0.0`，共享数据放在 `.volume/tools/report/`。

同一包各版本组成可以不同。每个含后端的已安装版本保存自己的精确版本 WASM 文件，含前端的版本保存自己的精确版本前端目录。活动版本包含前端时，宿主按登记打开前端目录。实际组成由每个版本自己的已验证清单决定。

前端路径采用去掉开头 `std/` 的包名，保留其余目录层级。标准包 `std/medical` 的前端实体在 `app/medical.<version>/`，入口为 `/app/medical/`。包登记、精确依赖、后端 WASM 及其业务数据仍按完整包名定位。

宿主可以从旧目录读取已安装前端，在该包下次安装或切换时用原恢复事务迁移。旧签名制品的 `/app/std/…` 资源地址继续解析到同一应用，使用新入口的访问规则，已存 ACL 规则保留原路径。同一前端路径只允许一个包提供。

### 4.3 原包与手册

原始 WAZ 按 Jade 的 `<完整包名>-<版本>.waz` 命名，直接保存在包管理目录。例如 Message 0.3.0 的原包路径是 `pkg/std/comm/message-0.3.0.waz`。以 `database/` 或 `releases/` 开头的包名使用 `database~/` 或 `releases~/` 目录，避开登记文件和旧缓存目录。读取清单、手册及其他交付文档时，宿主按安装登记中的完整 ZIP 摘要校验原包。

每个已安装版本及尚在恢复窗口内的事务都持有对应原包的保留引用。最后一个引用解除且文件事务持久完成后，宿主回收原包。缺少新路径时仍可读取旧目录中的原包。下次包事务完成后，宿主校验仍有引用的旧缓存并转入新路径，回收空的旧缓存目录。

包详情按所查询版本读取其原包中的 `docs/manual.md` 和 `[docs]`。默认详情指向活动版本。磁盘用量按 WASM 文件、前端目录和原包缓存计数。同一身份内多个依赖者复用同一个精确版本的运行实体。

## 5 多版本依赖解析

### 5.1 并存示例

```text
A 1.0.0 -> std/comm/message 0.2.2 -> message.wasm.0.2.2
B 1.0.0 -> std/comm/message 0.3.0 -> message.wasm.0.3.0

package_active(std/comm/message) -> 0.3.0
```

A 的调用选择 0.2.2，B 的调用选择 0.3.0。直接访问 Message 且采用默认选版的请求选择 0.3.0。

安装解析使用 `(完整包名, 精确版本)` 标识依赖节点。两条边指向同一节点时复用实体和已编译资源。不同版本形成独立节点。依赖闭包中的每条边都保留其源包版本和目标包版本。

依赖环按精确版本节点检测，并返回组成该环的依赖链。缺失、作者冲突和摘要冲突的错误同样包含对应依赖链。

### 5.2 WASM 内部调用

宿主为每次执行绑定经过验证的当前包名、版本和依赖闭包快照。同身份 WASI HTTP 请求先按现有路径分派规则确定目标包，再使用当前包的直接依赖确定目标版本。

选版顺序如下。

1. 目标属于当前执行包时，使用当前执行版本。
2. 目标属于直接声明的依赖时，使用该依赖的精确版本。
3. 目标属于其他业务包时，使用显式请求的已安装版本，缺省时使用活动版本。
4. 原生接口继续由现有原生 Router 处理。

第 3 项延续既有按路径调用其他已安装业务的能力。需要稳定版本的调用通过精确依赖声明绑定。WASM 显式携带的版本选择必须与其已绑定的自身或直接依赖版本一致，冲突时返回 `409 dependency_version_conflict`。

进入被调用包后，宿主使用被调用包自己的依赖关系。A 调用 D 0.2.2 后，D 对 E 的调用按 D 0.2.2 的清单解析。

宿主通过内部请求上下文传递调用来源。外部请求进入时由接入层初始化来源，WASI 出站接缝按实际执行版本设置来源。请求头中的调用者自述仅作为普通输入处理。

每次顶层执行持有其依赖闭包的资源快照，在途执行的后续依赖调用继续使用该快照。安装切换后，新执行取得新的快照。资源退役沿用既有取消及宽限契约。

### 5.3 跨身份调用

远端身份根据自己的安装集合选版。调用方可以显式声明所需的远端精确版本，缺省时使用远端活动版本。本地依赖绑定只对本身份内的调用直接生效。

## 6 HTTP 版本选择

### 6.1 精确版本字段

提议使用以下请求字段表达目标业务包的精确软件版本。

```http
Pishoo-Package-Version: 0.2.2
```

目标包由业务路径确定。字段接受一个符合现行包版本校验规则的完整版本字符串。重复字段、多值、范围表达式和格式错误返回 `400 invalid_package_version`。

外部请求携带该字段时选择对应已安装版本。缺省时选择活动版本。显式版本缺失返回 `406 package_version_unavailable`。活动版本缺失且请求采用默认选版时返回 `503 package_inactive`。

业务响应携带实际执行版本。缓存使用合并后的 `Vary`，ETag 与对应版本或内容绑定。

```http
Pishoo-Package-Version: 0.2.2
Vary: Pishoo-Package-Version
```

原生路径按各自现行协议处理。浏览器跨来源读取版本字段时，同步在既有 CORS 允许来源策略内处理该字段的预检和响应暴露。

此字段选择已安装软件版本。包自身的业务协议兼容规则继续由对应版本处理。

### 6.2 浏览器调用

Jade 构建和业务前端根据精确依赖声明生成或维护版本常量。A 的前端调用示例如下。

```javascript
fetch('/std/comm/message', {
  method: 'POST',
  headers: {
    'Content-Type': 'application/json',
    'Pishoo-Package-Version': '0.2.2',
  },
  body: JSON.stringify(message),
});
```

B 使用自己的依赖版本。宿主从显式请求字段选版，按真实访问主体执行权限检查。页面地址和 Referer 可用于日志，包版本绑定来自经过校验的内部上下文或显式版本字段。

SSE 使用支持设置请求头的流式 fetch 客户端，并在重连时携带同一个精确版本。原生 EventSource 的客户端接入需要迁移到该客户端。分页和重试同样保留原调用的选版。

需要绑定特定远端版本的持久任务，在自己的业务记录中保存目标版本，并在每次投递时携带。历史任务沿用原有缺省选版行为。共享队列中的任务格式需要纳入业务数据兼容设计。

### 6.3 路径分派

标准业务路径来自每个版本内嵌 OpenAPI，对外添加 `/std`。普通业务继续使用现行包名前缀规则。目标版本接收的 URI 转换与现行分派一致。

安装时检查所有保留版本的声明路径。同一包不同版本可复用同一路径。不同包之间按现有标准路径占用规则检查冲突，原生保留路径检查同样执行。

宿主先定位路径所属包，再选择版本，最后检查该版本的路径和方法。选中版本缺少路径返回 404，路径存在而方法缺失返回 405，并按该版本生成 Allow。

## 7 活动版本和前端入口

每个包的活动版本同时决定默认 Lib 和 App。默认 Lib 由活动登记直接定位精确版本入口。切换活动版本时，宿主将活动登记、权限声明投影和内存运行集合一起提交。

用户显式安装根包时，该版本成为该包的活动版本。安装依赖时，已有活动版本继续使用原选择。首次安装某个依赖时，以本次安装版本建立初始活动记录。

例如 A 已依赖 Message 0.2.2，再安装依赖 Message 0.3.0 的 B 时，两个版本同时安装，Message 的活动版本仍可保持 0.2.2。B 的调用通过自己的精确依赖选择 0.3.0。

App 地址使用第 4.2 节的前端路径，提供活动版本的前端。A、B 各自的前端可以同时通过显式版本字段访问不同的 D。仍有保留引用的版本可直接切换，已回收的版本需要重新安装。

App 静态资源读取直接取得活动版本的实体目录句柄，再按原有静态文件规则读取。App GET/HEAD 鉴权沿用完整包名根路径，资源、页面深链接和现有缓存处理由静态服务负责。

## 8 共享数据库和业务资源

### 8.1 唯一数据归属

Message 两个版本的 WASI `/db` 都挂载到以下目录。

```text
.volume/std/comm/message/
├── database
└── .file/
```

其他业务包沿用自己的真实数据库文件名。宿主按完整包名解析 volume，同一包的版本选择结果共同指向这一个目录。

原生 Message 附件索引和 WASM 继续打开同一个业务数据库，`native_files` 与业务表在其中共同保存。原生连接和 WASM 使用现有 `unix-dotfile` 目录锁协议。业务 volume 的数据库、附件、进度和持久队列共同参与跨版本数据兼容。

### 8.2 并发与迁移

各版本使用同一 SQLite VFS、锁协议和事务语义。初始化与结构迁移在业务数据库写事务中检查当前结构，再执行迁移。事务失败时回滚结构和迁移记录。SQLite 在正常隔离设置下向其他连接暴露已提交事务。[SQLite 隔离说明](https://www.sqlite.org/isolation.html)

业务包负责识别可读写的数据结构。新版结构需要让并存的旧代码继续正确读写。软件版本和数据库结构演进分别管理，由业务迁移逻辑保存必要的结构事实。

可并存的演进包括有兼容默认值的字段、新表，以及保留旧读写语义的兼容映射。数据语义发生变化时，业务包必须说明旧版写入与新版写入如何互相表示。

需要使用兼容视图时，同时实现读取及写入映射。SQLite 视图的写入适配可以使用 INSTEAD OF 触发器，触发器与视图在同一迁移中建立。[SQLite CREATE VIEW](https://www.sqlite.org/lang_createview.html)

共享持久任务的领取、状态更新和幂等检查在同一业务事务内完成，避免两个代码版本同时投递同一项任务。Message 的发送、附件提交和回执继续使用既有业务幂等标识。

### 8.3 数据兼容失败

遇到超出支持范围的数据结构时，对应业务版本在执行写操作前返回明确的兼容性错误。宿主可显示业务返回的错误和实际执行版本。

破坏性迁移需要业务包提供专门的升级步骤，包括暂停相关版本的访问、排空执行和事务、创建一致性备份、执行迁移及更新依赖者。日常包安装只进行静态校验与编译，数据库迁移由业务执行流程触发。

代码回滚切换运行版本及依赖绑定。业务数据库沿用当时已提交的数据，能否由旧代码继续读写取决于兼容性设计。需要恢复数据库时，执行独立的数据恢复操作并协调全部共享读写者。

## 9 安装登记结构

现行 package 表以 name 为主键，只能保存一个版本。多版本方案需要迁移登记结构，现行名称、作者与签名事实逐行保留。

### 9.1 包版本记录

`package` 改为以 `(name, version)` 为主键，保留现有元数据、组成标志、作者 owner 和签名证据。安装逻辑在同一写事务中验证同名全部版本的维护作者及 owner 连续性。

同一 `(name, version)` 已存在时，摘要相同的本地安装可以复用记录和实体。摘要不同返回 `409 package_content_conflict`。商店上传仍按现行重复版本规则执行。

### 9.2 活动记录与根包

`package_active` 保存 `(name, version)`，以 name 为主键，并通过复合外键引用已安装版本。

`package_root` 保存用户显式选择安装的根包 `(name, version)`，以 name 为主键。根包切换版本时更新这一记录。依赖安装生成版本记录和依赖边。

活动记录同时作为资源保留引用。显式根包、活动版本及它们的依赖闭包共同构成清理时的保留集合。

### 9.3 精确依赖边

`package_dependency` 保存以下字段。

```text
package_name
package_version
dependency_name
dependency_version
expected_author       可空，用于历史带作者依赖
```

主键为 `(package_name, package_version, dependency_name)`。源和目标都通过 `(name, version)` 复合外键引用 package。

各正式版本的依赖边与其已验签清单一致。预览产生临时依赖快照，并归属于预览会话。

### 9.4 权限声明投影

能力声明按 `(包名, 版本, 能力 ID)` 保存，操作记录带对应版本。权限目录投影已安装版本的路径并集，同一操作优先采用活动版本的能力归属。包详情按指定版本展示 OpenAPI，缺省展示活动版本。

现有 `UNIQUE(method, path)` 需要适配同包多版本重复声明。跨包路径占用继续在整组安装事务中检查，冲突检查考虑保留版本的声明集合。

实际访问授权仍按真实主体、方法和业务路径执行。同一路径授权覆盖其可选择的已安装版本，权限界面需要明确这一语义。

### 9.5 结构迁移

包登记 v5 采用复合主键，并提供从既有 v3、v4 登记的显式迁移。迁移在同一事务内重建主键和外键，校验历史精确依赖并建立活动记录。

历史登记无法证明根包来源时，将既有已安装包全部建立为显式根包，保留原有资源，后续由用户调整。

包管理查询紧邻对应数据库操作，连接检查和临时视图由连接初始化流程管理。完整初始化结构保存在 `src/package/schema.sql`，必要升级保存在版本迁移文件中。

安装库另设 `package_transaction`，保存尚在文件恢复窗口内的事务 ID。每次正式安装、版本切换和卸载，在同一 SQLite 事务中写入对应 ID。它用于区分登记事实相同的重复安装和资源修复是否完成提交，恢复日志清理完成后回收该 ID。

## 10 安装提交和崩溃恢复

### 10.1 准备

1. 解析根包及精确依赖闭包，完成作者、签名、回执、CRL、摘要和包格式验证。
2. 将新增实体展开到身份内同文件系统的临时目录，拒绝归档中的路径逃逸和特殊文件。
3. 校验交付文件，编译候选 Lib，打开候选前端目录句柄。
4. 生成整组计划，记录新增版本、活动切换、依赖边、运行文件和前端目录。

### 10.2 提交

提交沿用现有 Sandbox 写锁、身份目录锁及跨安装登记和能力声明的 SQLite 事务。宿主管理写操作依次取得 Sandbox 写锁、身份目录独占锁，再进入数据库写事务。同时需要 Sandbox 和文件发布状态的读操作按对应顺序取得 Sandbox 读锁及身份共享锁。Jade watcher 仅取得身份目录锁，文件锁覆盖入口发布。

1. 取得写锁后重新检查依赖、作者、路径冲突和预览归属。
2. 写入持久恢复日志，保存旧登记事实、目标登记事实、各资源的类型和路径。
3. 同步新增 WASM 文件和前端目录，逐项发布到最终 lib 与 app 路径。需要替换的旧资源移入本次事务备份位置。
4. 清理本次涉及的旧版 App 链接，并将其纳入同一恢复事务。
5. 提交包版本、依赖边、活动记录、根包、权限声明及本次事务 ID。
6. 在仍持写锁时一次发布新的内存运行集合，持久清理恢复日志，再回收事务 ID 和临时目录。

单个文件发布可通过 rename 原子完成，多个运行资源与 SQLite 之间的一致性由上述锁和恢复日志共同管理。已存在前端目录的替换通过事务备份和恢复日志协调。打开的文件描述符在路径替换后仍然有效。[rename 手册](https://man7.org/linux/man-pages/man2/rename.2.html)

切换活动版本时，保留的各版本继续承接其精确版本请求。版本卸载、预览构建替换和宿主关闭触发对应资源退役，取消与宽限沿用现行执行契约，范围按版本与预览构建精确区分。

### 10.3 崩溃恢复

启动先恢复文件事务，再加载执行资源。恢复日志通过事务 ID 判断数据库提交结果，并核对本次涉及的完整版本、摘要、活动选择和依赖事实。

宿主在启动时持身份目录独占锁，清理符合安装器命名且没有恢复日志的暂存目录。原包缓存只因存在待恢复的日志而延后回收。

事务 ID 缺失时恢复旧运行文件、前端目录和 App 链接，ID 已提交时补齐新资源。修复后重新验证登记与资源一致性，持久移除文件日志后，再回收数据库中的事务 ID。遇到提交标识与登记事实冲突的损坏时，保留日志并报告具体冲突。

已经发布而尚无登记引用的运行实体进入后续清理候选。运行资源发生外部修改时，启动通过原包和登记校验，使用可验证的缓存恢复资源并记录原因。

## 11 运行文件读取和活动前端选择

WASM 普通文件及前端目录内部继续通过目录句柄和 `O_NOFOLLOW` 受限打开。App 从活动登记推导前端目录。[当前文件打开实现](../src/package/files.rs)

1. 从已打开的身份目录句柄定位登记中的预期包、版本和作者。
2. 根据精确版本推导 lib 普通文件路径，通过目录句柄打开并在最终文件句柄上校验内容。
3. 根据活动版本和 App 路径推导前端版本目录。
4. 从 app 根目录句柄逐段打开前端目录和内部文件，执行现有目录和文件类型检查。
5. 预览覆盖时根据有效归属记录解析当前运行资源及被保留的正式资源。

前端目录内部继续执行逐段受限遍历。包格式读取器只接收普通交付文件。旧版安装的活动链接可以留在磁盘，直到涉及该包的安装、切换或卸载事务清理。

宿主以文件句柄执行读取、验签、编译和所有权操作，规避先解析字符串后重新打开路径造成的替换竞态。读取锁覆盖入口选择及句柄获取。

## 12 Jade 预览

### 12.1 快照布局

只有实际的 `dev` 子身份启用 Jade 预览，例如 `dev.alex.chen`。预览访问要求真实主人。包沿用项目版本，WASM 和前端分别使用正常的版本化文件与目录。宿主从 `lib/<完整包名>.preview/manifest.json` 发现项目，并核对 `state.json` 记录的文件身份。App 路由及其 ACL 使用同一挂载路径。

### 12.2 已安装依赖

开发身份可以运行已安装的依赖。正式包和预览项目使用各自的目录发现规则，同一项目的预览与正式安装分属不同身份。正式读取从安装登记定位运行资源。 预览 Lib 和已安装 Lib 共用业务路由与精确版本选择。不同标准包声明同一路径返回 409，选中预览时校验真实主人。

### 12.3 更新与退出

Jade 从临时目录发布完整构建并更新归属记录。宿主按实际文件和依赖更新预览缓存，并退役旧构建。Jade 退出时仅清理仍属于自己的文件，业务 volume 留在开发身份中。

## 13 管理接口和卸载

### 13.1 安装和查询

现有安装输入继续使用 name、精确 version 和可选 author。已安装且摘要相同的版本可从本地已验证缓存复用，显式安装同时完成根包和活动版本切换。

包清单按包名展示活动版本，同时增加已安装版本列表和各版本的依赖者。详情查询增加可选 version。缺省详情继续返回活动版本的现有字段，指定 version 时返回该版本的事实、手册和 OpenAPI。

| 请求 | 行为 |
| --- | --- |
| `GET /std/sys/pkg/list` | 列出包、活动版本和已安装版本 |
| `GET /std/sys/pkg?name=std/comm/message` | 查询活动版本详情 |
| `GET /std/sys/pkg?name=std/comm/message&version=0.2.2` | 查询精确版本详情 |
| `POST /std/sys/pkg/install` | 安装精确版本并将显式根包切换至该版本 |
| `DELETE /std/sys/pkg?name=std/comm/message&version=0.2.2` | 卸载指定版本 |
| `DELETE /std/sys/pkg?name=std/comm/message` | 卸载整个包 |

Workspace 的包管理页提供版本列表、依赖者、切换活动版本和卸载指定版本。切换活动版本复用现有精确版本安装入口。

### 13.2 卸载

现有按 name 卸载表示卸载该包全部版本。指定 version 时卸载单个版本。所有删除在身份目录锁和安装事务内执行。

其他已安装包版本仍引用目标版本时返回 `409 package_in_use`，列出精确依赖者。指定删除活动版本时返回 `409 package_active`，用户可以先通过安装入口选择另一个活动版本，或明确执行整个包的卸载。

卸载提交移除目标版本登记、依赖边、对应 WASM 文件、版本前端目录和资源引用。整个包卸载同时移除活动记录及前端入口。原包缓存根据保留策略回收，业务 volume 和主体授权按现行卸载语义保留。

### 13.3 资源清理

安装或切换活动版本时，从更新后的显式根包、活动版本、Jade 预览及其声明的依赖遍历精确依赖闭包。其他已登记的版本在同一恢复事务中退役。先移除退役版本的所有出站依赖边，再删除对应登记和运行实体，循环依赖可一并清理。提交失败时保留原登记和运行资源。成功提交后，退役版本沿用现有运行取消与宽限机制。

安装、切换和卸载的恢复日志持久清除后，宿主回收包管理目录中无登记引用的原包。身份下存在尚待恢复的文件事务时延后回收。清理范围限于包登记和对应程序资源，业务 volume 与访问授权沿用卸载约定。

## 14 现有安装迁入

历史 WAZ v1 继续按原 JSON 清单、目录和签名字节验证。宿主按历史交付条目读取 WASM、前端和手册，再映射为对应的运行文件与版本前端目录。WAZ v2 使用第 3 节的包内路径。

迁移按照以下顺序执行。

1. 迁移单版本登记，建立该版本的活动记录与根包记录。
2. 用现有原包缓存验证当前资源，或在显式安装该版本时完成资源重建。
3. 在文件恢复事务内把旧 `message_0.2.2.wasm` 普通文件迁为 `message.wasm.0.2.2` 普通文件，并核对活动登记。
4. 将现有 App 文件与已验证版本核对后，迁入对应版本前端目录，并登记活动版本。
5. 完成整组登记、资源和内存集合切换后，清理本次接管的旧资源。

缺少可验证原包的既有安装可以通过旧读取路径继续运行，显式安装时进入新布局。

旧 `%2F` 路径及 `.volume/std/message/` 沿用已批准的迁移规则，新目标缺省时迁入完整包名目录，迁入时连同数据库 sidecar、附件和锁相关资源整体协调。

## 15 实施范围和冻结接口差异

| 模块 | 需要落实的变化 |
| --- | --- |
| Pishoo package | 多版本登记、精确依赖边、活动版本、根包、原包复用与迁移 |
| Pishoo files | 版本运行文件、版本前端目录、活动前端选择、整组发布、恢复与清理 |
| Pishoo Sandbox | Lib 集合按包名和版本索引，版本级加载与退役 |
| Pishoo Invocation | 绑定当前执行包版本与依赖闭包快照 |
| Pishoo Outgoing | 同身份调用按执行来源绑定目标版本 |
| Pishoo routing | 外部精确选版、跨版本路径检查、响应版本及缓存语义 |
| Pishoo development | 版本预览覆盖、构建归属、正式和开发请求各自解析 |
| Jade | Linux 风格版本入口、预览快照、清理归属、前端依赖版本接入 |
| Workspace | 版本列表、依赖者、活动切换、指定版本详情及删除 |
| 业务包 | 共享数据库并发兼容、迁移、请求选版、SSE 与任务幂等 |
| 商店 | 继续提供精确版本事实和完整原包，按现行发布协议校验 |

冻结差异至少覆盖以下事项。

- Sandbox.libs 从完整包身份索引扩展为包名和精确版本索引，预览缓存同时区分项目版本与构建。
- Invocation 和出站执行上下文携带真实源包版本及依赖资源快照。
- 包文件读取和已安装资源查询增加精确版本选择，更新对应跨模块函数签名。
- 安装登记增加复合主键、活动记录、显式根包和版本化能力声明。
- 包管理查询与删除增加可选版本选择，业务 HTTP 增加精确版本字段。

具体 Rust 类型、成员和函数签名记录在宿主冻结清单中，安装、启动、调用和预览入口共同使用这些定义。

## 16 实施顺序

1. 更新包登记结构及迁移，打通旧安装读取和多版本事实查询。
2. 实现版本 WASM 文件、版本前端目录和恢复日志，验证安装、卸载与故障恢复。
3. 按版本加载资源，接入调用来源、依赖快照与同身份选版。
4. 接入外部精确选版、前端依赖版本和版本化接口目录。
5. 更新 Jade 快照发布、预览覆盖和归属恢复。
6. 验证各业务包共享数据库读写，更新 Workspace 管理界面与业务调用。

## 17 验收条件

1. A 精确依赖 D 1.0.0，B 精确依赖 D 2.0.0，两个版本同时安装并分别执行。
2. 切换 D 的活动版本后，A、B 的精确依赖绑定保持各自声明的版本。
3. 同一 D 的两个版本读写同一数据库和附件目录，另一个版本可以读取已提交且兼容的数据。
4. 两个版本同时推进持久任务时，业务幂等标识和事务领取逻辑维持正确状态。
5. 已安装版本的显式请求、缺省请求、无可用版本、路径缺失和方法缺失分别返回约定结果。
6. 新旧代码同时执行兼容迁移时，迁移只提交一次，失败事务完整回滚。
7. 原生附件索引和 WASM 通过相同锁协议访问同一业务数据库。
8. A 依赖 D 1.0.0，D 1.0.0 依赖 E 1.0.0，多级调用逐层选择正确版本。
9. 浏览器 A、B 的分页、附件、SSE 和重试持续选择对应的精确版本。
10. 每个提交步骤注入失败或中断后，重启可恢复到完整的旧状态或新状态。
11. 将版本前端目录或 WASM 文件替换为链接时，受控打开路径拒绝该资源并报告布局错误。
12. 身份目录整体移动后，宿主仍按登记打开运行文件和前端版本目录。
13. 真实主人可在开发子身份访问预览，其他身份沿用安装登记。
14. watcher 退出时按当前归属清理预览资源。
15. 存在精确反向依赖时阻止卸载目标版本，解除依赖后可按版本清理。
16. 历史 WAZ v1、旧版本文件名、编码路径和旧 Message volume 按迁移规则读取并进入新布局。
17. 旧代码遇到超出支持范围的数据结构时，在业务写操作前报告兼容性错误。

实现验收覆盖宿主、Jade 和业务的正常入口。测试代码分别放入所属仓库或 crate 的 tests 目录。

## 18 运行一致性

启动读取每个精确版本的 WASM，并核对其 OpenAPI 版本与安装登记一致。构建调用图发生错误时保留当前完整运行集合。业务 volume 的挂载按完整包名解析。

开发预览更新时，watcher 在身份目录锁内核对上次发布的文件身份。宿主根据当前预览文件更新运行缓存。

## Package-name transition and atomic updates (2026-11-18)

Temporary compatibility is isolated in `src/package/renames.rs`. Only the exact
`dev.genmeta.tech.dhttp.net` author is eligible; `std.dhttp.net` is excluded.
The explicit mappings are profile → std/profile, message or comm/message →
std/comm/message, comm/group → std/comm/group, calendar → std/calendar,
medical → std/medical, command → std/command, and python → std/python.
There is no generic prefix stripping or cross-author transfer.

The installation endpoint additionally accepts a batch of roots:

```json
{"packages":[{"author":"dev.genmeta.tech.dhttp.net","name":"workspace","version":"0.3.13"},{"author":"dev.genmeta.tech.dhttp.net","name":"std/calendar","version":"0.1.5"}]}
```

`POST /std/sys/pkg/install` accepts 1–100 distinct roots within a 64 KiB JSON
body. The complete exact dependency closure remains bounded to 100 releases.
It returns `{"packages":[...]}` in root request order. Every explicitly requested
root becomes active, and all downloads, signatures, owner checks, compilation,
route checks and dependency validation precede the existing atomic publication.
Single-root requests retain their existing response and cached activation path.
Example versions are illustrative and must be checked before publication.

A renamed package retires all installed predecessor versions only after owner
continuity is verified. Historical dependent releases are retired together;
an active or explicitly installed dependent must be replaced in the same batch,
otherwise `package_in_use` reports the package that also needs updating. Exact
signed dependencies are never rewritten to satisfy a rename. Retired runtime
calls are cancelled only after commit. The host reclaims signed WAZ caches once
no installed version or recovery journal needs them.

Program replacement and the Message volume move share the existing recovery
journal. Compilation opens the old volume handle; publication moves the
`std/message` directory to `std/comm/message`, retaining the inode, database,
attachments and native file index. Failed compilation does not move data;
failed commit and interrupted publication restore the old location. If both
directories exist, installation fails for explicit reconciliation. Other
standard package renames already use the same volume directory. ACL rules are
not broadened, and moved App entry paths must be reviewed when sharing access.
Business database migrations remain the responsibility of their package;
package transaction rollback does not roll back later business writes.

After the transition window, remove the isolated rename module and its call
sites in package acquisition/installation/publication recovery. Finish pending
publication recovery before removing compatibility. Generic batch installation
remains supported; no alias table or schema migration needs removing.

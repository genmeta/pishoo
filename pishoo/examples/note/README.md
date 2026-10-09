# Note WASM 便签设计

Note 是由一个 WASI HTTP component 提供的个人便签工具，支持手机和电脑新建、编辑、搜索和删除便签。页面和 API 都由 `lib/note/lib.wasm` 返回，数据库为 `<身份目录>/db/note/note.db`，该 Lib 的私有目录挂载为 `/db`。Pishoo 继续使用现有组件加载、路由、WASI 文件能力和 daccess 授权。

本文件定义第一版实现及验收方式。访问入口为 `https://<身份名称>/std/api/note/`；手机使用支持 DHTTP 的浏览器。

## 页面与操作

页面名称为 `note`，采用系统字体、纯文本编辑和适合手机的单列布局。HTML、CSS、JavaScript 编入同一个组件，页面只请求同源 API，不加载 CDN。

列表页顶部放搜索框和“新建”按钮，下面按更新时间降序显示便签。每项显示标题、正文摘要和更新时间；同一更新时间按 id 排序。搜索匹配标题和正文，清空搜索恢复全部便签。空列表显示“写下你的第一条便签”，搜索为空显示“没有找到便签”。

打开便签后进入编辑页：顶部为“返回”和“保存”，下面为标题输入框、正文输入框、保存状态和“删除”按钮。电脑宽屏使用左侧列表、右侧编辑；手机只展示当前列表或编辑页，正文输入框保持可用高度，交互目标至少 44px，字号至少 16px。

保存由用户明确触发。修改后显示“未保存”，提交中显示“正在保存”，成功后显示服务端返回的保存时间；提交期间保留输入并暂时禁止编辑和重复提交。标题为空时，列表显示正文第一行；标题和正文都为空时不允许保存。

从有未保存内容的编辑页返回、切换便签或新建其他便签时，提供“保存后离开 / 放弃修改 / 继续编辑”。页面关闭使用浏览器支持的离开提示；浏览器可能不显示提示，不把它作为数据保存保证。页面加载失败提供重试；保存失败保留当前输入，不能显示“已保存”。

删除需要确认，成功后返回列表。删除失败保持当前内容。第一版删除在界面上不可撤销；存储中的旧版本不是对外提供的回收站。

## HTTP 接口

组件清单使用 OpenAPI 3.1，仅声明固定路径。当前 manifest 校验拒绝路径模板，因此单条便签通过 query 中的 `id` 定位，不使用 `/notes/{id}`。

| 方法 | 组件内路径 | 对外路径 | 用途 |
| --- | --- | --- | --- |
| GET | `/` | `/std/api/note/` | 返回完整页面 |
| GET | `/notes` | `/std/api/note/notes?q=文字` | 返回便签列表或搜索结果 |
| POST | `/notes` | `/std/api/note/notes` | 新建便签 |
| GET | `/note` | `/std/api/note/note?id=…` | 读取一条便签 |
| PUT | `/note` | `/std/api/note/note?id=…` | 保存已有便签 |
| DELETE | `/note` | `/std/api/note/note?id=…` | 删除便签 |

页面只使用相对路径 `notes` 与 `note`，不包含宿主命名空间或 LibId。Pishoo 将清单内 `/notes` 挂到 `/std/api/<LibId>/notes`，并把已声明 GET/HEAD 的无尾斜杠 Lib 根入口307跳转到带尾斜杠的入口，保留 query；浏览器因此自动把页面相对请求拼到当前 Lib 入口下。Pishoo 交给 guest 的请求再去掉外部挂载前缀，guest 按上表的组件内路径处理。更换 LibId 或宿主前缀无需改页面。

JSON API 的请求为 `application/json`；响应为 `application/json; charset=utf-8`，页面为 `text/html; charset=utf-8`。页面和数据响应设置 `Cache-Control: no-store`。列表返回 `{ "notes": [...] }`，每项只有 `id`、`title`、`preview`、`updated_at` 和 `version`；列表不返回全部正文。

单条便签的 JSON 形状如下，时间由 guest 通过 WASI wall clock 生成，单位为 Unix 毫秒，页面按手机时区显示：

```json
{
  "id": "e0b8713acdb84942b878b88e2c343264",
  "title": "周末要办的事",
  "body": "买咖啡豆\n整理照片",
  "created_at": 1791446400000,
  "updated_at": 1791446400000,
  "version": "1"
}
```

id 为 16 个随机字节的 32 位小写十六进制编码，创建后不变。version 是十进制字符串，避免浏览器整数精度限制；它只表示这条便签的提交顺序。created_at 创建后不变，updated_at 为此次提交时间；文件系统与版本决定并发顺序，不依赖时钟单调递增。

新建请求为 `{ "id": "…", "title": "…", "body": "…" }`。页面在首次提交前生成并保留随机 id，以便响应丢失时通过该 id 查询保存结果。创建成功返回 `201`、便签 JSON 和 ETag；id 已使用则返回 `409`，不能覆盖已有便签或复活已删除便签。

读取成功返回 `200`、便签 JSON 和强 ETag，例如 `"1"`。更新请求只包含 `{ "title": "…", "body": "…" }`，并携带 `If-Match: "1"`。成功返回 `200`、新便签 JSON 和新的 ETag。删除也携带 If-Match，成功返回 `204`。

缺少 If-Match 返回 `428`，版本落后或提交时发生竞争返回 `412`。页面在冲突时保留本地草稿，提示“这条便签已在其他设备修改”，允许查看当前服务器内容和复制本地草稿；用户主动合并后使用新版本保存，不自动用旧正文覆盖新版本。被删除的便签返回 `404`，不能通过普通更新重新创建。

输入错误返回 `400`，不支持的请求类型返回 `415`，正文超过限制返回 `413`，存储错误返回 `500`。guest 的错误响应为 `{ "error": "稳定错误代码", "message": "可读说明" }`。daccess 的拒绝或审批响应来自现有宿主，页面同时处理非 JSON 响应：`403` 显示无访问权限，`202` 显示等待授权，不把它们当作保存成功。

## SQLite 存储与并发

宿主为每个 Lib 挂载 `<身份目录>/db/<LibId>` 为 `/db`。Note 在 guest 中打开 `/db/note.db`，实际文件为 `<身份目录>/db/note/note.db`。两个身份各有自己的数据库；宿主根级 config.db、access.db、workspace.db、chat.db 和其他 Lib 的目录均不可访问。

Note 使用 guest 内编译的 SQLite，与页面及 API 一起打包进 component。每次请求打开现有 rusqlite Connection，处理完毕关闭；不在 Pishoo 中增加数据库宿主 API、连接池、锁字段或 Lib 并发名额。数据库由首次数据请求初始化，user_version=1；已有文件损坏、非空未知 schema 或不支持的版本返回错误，不能当作空库覆盖。

```text
<身份目录>/
  lib/note/lib.wasm
  db/note/
    note.db
    note.db-journal              SQLite 事务期间可能存在
    note.db.lock/                SQLite unix-dotfile VFS 的锁目录
```

notes 表保存 id、title、body、created_at、updated_at、version、kind。kind 为 note 时 title/body 必须非空值，kind 为 deleted 时 title/body 为 NULL；删除保留 id 与递增版本作为墓碑，防止旧创建请求复活便签。接口中的 version 为字符串，数据库内为正的有符号 64 位整数；版本溢出返回存储错误。

创建采用唯一主键插入。更新与删除在 SQLite 即时事务中使用 `WHERE id=? AND version=? AND kind='note'`；只有匹配当前版本的一个请求能修改记录。修改行数为零返回 412；不同便签的写入也由 SQLite 数据库锁串行提交。SQLite 忙超过两秒返回 503，页面保留草稿供重试，不添加宿主准入控制。

WASI 缺少常规 POSIX 文件锁，沿用 SQLite 自带的 unix-dotfile VFS，以现有文件系统目录操作协调独立 Store。使用 rollback journal 和 synchronous=FULL；编译关闭 WAL，避免依赖跨实例共享内存。运行时不选择无锁 VFS。正常连接关闭会释放锁；若进程在持锁期间异常退出，可能留下 note.db.lock，后续请求会报忙。只有确认相关进程已经停止后，才可离线清理遗留锁并让 SQLite 恢复 journal；不按时间自动删除锁。

响应丢失或提交报错时，页面显示保存结果未确认，按保留的 id 重新读取并核对正文；不能把网络错误解释为服务器一定未保存。备份时保留 SQLite 必需的恢复文件，或在停止服务后复制数据库。

## 内容与权限

第一版便签是个人纯文本内容。正文始终通过 textarea/value 或 textContent 展示，不作为 HTML 执行。搜索词按字面匹配，不解释成正则表达式。id 必须严格符合编码规则，不能作为任意文件路径使用。

标题最多 200 个 Unicode 字符，正文最多 64 KiB UTF-8，请求体最多 128 KiB，搜索词最多 200 个 Unicode 字符。读取请求时逐块检查大小，超过限制立即返回错误；历史数据读取也检查文件大小和字段。这些是 Note 的输入限制，不增加 Pishoo 的传输配额或执行名额。

所有便签属于加载 Note 的这个 Server 身份。所有获准访问某个 Note 数据 API 的来访者访问该身份同一份便签库；第一版不设计每个来访者独立的数据分区。

页面与数据 API 均通过现有 daccess。实际部署先使用同名、同 owner 的身份验收；手机使用其他身份时，明确授权上表中需要的完整对外方法与路径。读取和写入权限可以分别授予。组件的 OpenAPI 清单不等于访问授权，Note 不自行注册匿名允许规则。

## 源码与打包

Note 使用独立的 guest crate，源码置于 `pishoo/examples/note/`，名称不带 demo。文件组织如下：

```text
Cargo.toml
Cargo.lock
src/note.rs              Cargo [lib].path 指向这里
page.html                页面及内联 CSS/JavaScript，构建时嵌入
openapi.json             上表中的固定路径与显式方法
README.md                本设计与实际部署说明
```

guest 使用 Rust wasm32-wasip2 目标与官方 WASI SDK 编译内嵌 SQLite，导出当前宿主使用的 `wasi:http/incoming-handler@0.2.12#handle`。组件包含恰好一个顶层 `pishoo:openapi` custom section；沿用现有 `package-lib.py` 打包工具；wasm32-wasip2 的编译结果已经是 component。Rust 文件使用普通 mod 与同名 `.rs` 文件，需要拆分时按页面、HTTP、存储职责拆分，不使用 include! 源码片段。

Note 不修改 Server、Sandbox、Lib、StoreData、Invocation、dhttp 或 h3x 的冻结结构与签名。HTTP handler 复用现有 WASI 资源，文件读写只使用现有逐 Lib 目录能力，在 `/db` 中读写；所有临时计算和序列化都属于当前请求。WASM 不发起出站 HTTP，页面通过手机浏览器调用同源 API。

## 实际验收

验收先编译真实 Note component，附加 manifest，并通过 `wasm-tools validate` 与 `check-lib`。随后在临时身份数据目录中用实际 Wasmtime 执行页面、创建、读取、更新和删除；并发验收驱动多个独立 Invocation，不能仅调用普通 Rust 存储函数。

| 场景 | 操作 | 预期 |
| --- | --- | --- |
| 页面载入 | 手机打开 `/std/api/note/` | 显示列表或空状态，无外部资源依赖 |
| 新建 | 输入中文、换行和 emoji，保存 | 显示已保存，重新读取内容完全一致 |
| 持久化 | 关闭页面后重开，再重启 Pishoo | 已保存的便签仍然存在 |
| 编辑与搜索 | 改标题和正文，分别搜索 | 新内容可读，列表排序与搜索正确 |
| 多设备冲突 | 手机和电脑打开同一版本，先后保存 | 后保存的旧版本请求返回 412，草稿保留 |
| 同时提交 | 两个独立 WASM 调用竞争同一版本 | 恰好一个下一版本提交成功 |
| 删除竞争 | 一个设备删除，另一个保存旧版本 | 只接受一个下一版本；旧保存不能复活删除 |
| 未保存离开 | 修改后返回或切换便签 | 出现保存、放弃、继续编辑选择 |
| 中断与失败 | 提交前后中断连接，模拟磁盘错误 | 旧记录或完整新记录可读，页面保留草稿并核对结果 |
| SQLite 提交能力 | 在实际 WASM 中同时保存并检查数据库完整性 | 版本检查生效，PRAGMA integrity_check 返回 ok |
| 权限 | 未授权手机身份访问数据 API | 由 daccess 拒绝，磁盘内容保持不变 |
| 隔离与输入 | 非法 id、超长正文、另一个 Lib 的路径 | 输入被拒绝，不能访问 Note 私有数据库目录之外 |

部署文件为 `<身份目录>/lib/note/lib.wasm`。数据库保留在 `<身份目录>/db/note/`；更换组件时保留该目录。Pishoo 当前仅在启动加载 Lib，安装或更新 Note 后必须重启。具体服务名、手机身份和授权规则在实际部署时确定。

## 构建与回归命令

先安装 Rust 的 wasm32-wasip2 编译目标和官方 WASI SDK，设置 SDK 路径：

```sh
rustup target add wasm32-wasip2
WASI_SDK_PATH=/path/to/wasi-sdk ./pishoo/examples/note/build.sh
cargo run --locked -p pishoo --example check-lib -- pishoo/examples/note/dist/lib.wasm
cargo test --locked -p pishoo --lib execution::note:: -- --ignored --skip note_browser_preview
```

构建输出为 `pishoo/examples/note/dist/lib.wasm`，dist 和 target 不入版本控制。实际回归执行完整组件，覆盖 CRUD、中文搜索、独立实例并发、重新加载后持久化、两个身份隔离、根级宿主数据库及其他 Lib 的目录越界拒绝。

需要本机浏览器验收时，运行以下显式忽略的测试；它仅使用临时目录和回环 HTTP，所有页面和 API 仍由真实 Pishoo Invocation 执行，不修改生产身份或授权：

```sh
cargo test --locked -p pishoo --lib note_browser_preview -- --ignored --nocapture
```

在五分钟内打开 `http://127.0.0.1:18743/std/api/note/`；临时数据随后删除。这不是部署入口，手机正式验收使用 DHTTP 身份地址。

## 已完成验收

2026-10-08：组件已编译并通过 WASM 与 Pishoo 清单校验。27 项 Sandbox 测试通过（含实际 Note 组件的 CRUD、重载持久化、八个独立 Invocation 竞争同一版本和逐 Lib 数据库目录隔离）；常规库回归 111 项通过。手机尺寸浏览器在真实 WASM 回环服务中验证了中文保存、保存状态与未保存离开提示。

正式安装后的两个身份分别通过真实 QUIC 验证页面与列表接口返回 HTTP/3 200，页面内容与编译时嵌入内容一致，各自的私有 SQLite 数据库通过 integrity_check。部署继续使用 daccess 的现有权限；不同名称的手机身份需要单独获得 Note API 授权。

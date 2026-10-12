# 手记 WASM

Journal 是一个 WASI HTTP component。Apple「手记」形式的个人主页、图文发布、照片、具名留言和回复，以及 SQLite，均在一个 `lib.wasm` 中。界面使用选定的「图文手记」方案，支持手机单列、系统明暗模式、月份分组和分页。

0.1.1 调整桌面布局：个人信息与导航放在左侧，手记卡片使用两列，宽屏阅读时正文与留言并排；手机继续单列。界面标题、当前身份、评论署名及回复输入只省略末尾 `.dhttp.net`，例如显示 `alice.smith`。API、存储、WIT 与身份/owner_hash 校验仍使用完整身份，不修改已有手记或留言。

## 身份与权限

组件通过用户批准的 `pishoo:identity/request-identity@0.1.0` 读取本端名称、已验证来访名称和主人判断。宿主直接使用每次 Invocation 的既有握手；不接受 HTTP 头、query 或 JSON 中的身份，不向 guest 暴露私钥、证书或 owner_hash。匿名请求，包括主页、数据和照片，均返回 403。主人判断同时检查规范化名称和证书 SKI owner_hash；仅同名但不同 owner 的来访者不能写入。

完整方法和路径仍由 Pishoo 的 daccess 层授权，组件不自动登记 Allow 规则。发布、编辑、删除手记和删除留言还要求 caller-is-owner；所有获准提交评论的具名身份可以留言和回复。作者来自 WIT，JSON 的 author/name 等未知字段被拒绝。留言显示的验证标识表示发布时的认证，不承诺历史作者的证书现在仍有效。第一版由主人删除留言，不提供来访者自己的删除接口。

上线前为需要访问的身份明确授权主页、context、数据和照片的读取，以及所需评论方法。未获准的身份仍由 daccess 拒绝或审批，页面处理 403/202，写入失败保留草稿。默认不公开内容，也不替换现有授权策略。授权按完整 API 方法/路径控制，第一版没有逐篇公开/联系人/私密开关；访问范围不能仅由界面标签决定。

写入使用同源 JSON 和非简单请求头 `x-journal-write: 1`；无 CORS 授权，拒绝跨站 Fetch 元数据及不匹配的 Origin。这个头只用于浏览器跨站写入防护，不能用于认证。页面和所有响应均为 no-store、no-referrer、nosniff 和 same-origin 资源策略；页面不加载外部字体、CDN、跟踪脚本或评论服务。正文和留言只作为转义后的文字显示，不执行上传的 HTML。

0.1.2 修正简写地址的 Origin 比较：`https://alice.smith~` 与请求中已展开的 `https://alice.smith.dhttp.net` 按同一主机比较，协议及有效端口仍须相同。裸 `~` 只按已验证来访身份展开，不当作任意目标的同源来源。不同主机、跨站标记、带路径/query/fragment/用户信息、无效或重复 Origin 仍拒绝。认证和评论作者仍完全来自 WIT。

浏览器将 JPEG/PNG/WebP 转成最长边 1600 像素的 JPEG，去掉原文件附带信息。guest 只接受 1 MiB 以内、最长边 2048 的 JPEG，并解析标记段，剥离全部 APPn/COM（包含 EXIF、GPS、XMP、缩略图和注释），再存储、发布。SVG 和外部图片地址不接受。第一版每篇一张照片。

## 入口与接口

安装到 `<身份目录>/lib/journal/lib.wasm`，入口为 `https://<身份名称>/std/api/journal/`，需要支持 DHTTP 的浏览器。网页只请求相对 URL，宿主统一挂载和去掉 `/std/api/<LibId>` 前缀。

| 方法 | 组件内路径 | 用途 |
| --- | --- | --- |
| GET/HEAD | `/` | 完整页面 |
| GET | `/context` | 当前已验证身份与主人判断 |
| GET | `/entries?offset=0` | 每页 20 篇，按发布时间倒序 |
| POST | `/entries` | 主人新建手记 |
| GET/PUT/DELETE | `/entry?id=…` | 读取、主人编辑/删除 |
| GET | `/photo?id=…` | 私有照片，仍经过授权 |
| GET | `/comments?entry_id=…` | 留言和一层回复 |
| POST | `/comments` | 当前身份留言或回复 |
| DELETE | `/comment?id=…` | 主人删除留言 |

新建内容的 id 是提交前生成的 16 随机字节十六进制字符串，服务端插入不可覆盖同名记录。编辑和删除使用强 ETag 与 If-Match；缺少版本返回 428，旧版本返回 412。照片更新与正文在同一事务提交，省略 photo 保留原图，显式 null 移除。重发创建返回 409，页面保留原 id，允许核对已保存记录；不把网络中断解释成一定没有提交。

正文最多 64 KiB，标题最多 200 字符，留言最多 8 KiB，每篇最多 500 条留言/回复，请求体逐块读取，最多 1.5 MiB。回复必须指向同篇手记的未删除顶层留言；删除留言清空其作者和正文，但保留回复所需的墓碑。删除手记同时清空照片/正文并删除留言，手记 id 留作墓碑，旧创建不能复活。删除不自动改写已有备份。

## 存储与生命周期

文件为 `<身份目录>/db/journal/journal.db`，guest 挂载为 `/db/journal.db`。不能访问宿主 access/config/workspace/chat 数据库或其他 Lib。每请求直接打开现成 rusqlite Connection，没有宿主数据库接口或连接池。

沿用 Note 已验证的 SQLite unix-dotfile VFS、rollback journal、synchronous=FULL 和即时事务，不使用无锁 VFS 或 WAL。并发更新只允许一个请求提交当前版本；锁忙两秒后返回 503。库损坏、未知非空 schema 或不支持版本拒绝操作，不能当成空库覆盖。进程异常退出可能遗留 `.lock` 目录；只有确认服务停止后才能离线清理，不按时间自动删锁。备份需在停止服务后复制数据库及恢复文件，或使用 SQLite 一致快照。

首次数据请求初始化 schema v1。组件更新保留私有数据库目录；Pishoo 当前在启动时加载 Lib，安装/更新需要重启。此组件需要包含新 WIT 的配套 Pishoo 宿主，旧二进制不能加载该导入。

## 构建与验证

身份调用使用标准 SDK 依赖 `pishoo-sdk`，源码直接 `use pishoo_sdk::request_identity`，不再在示例中生成 WIT 绑定。仓库依赖 `../../../wit`；`python3 pishoo/tools/package-sdk.py` 生成的独立开发包将依赖改成 `../../sdk`，包含本示例、SDK 和打包工具。SDK 尚未发布到 crates.io。使用说明见 [SDK 文档](../../docs/sdk.md)。

```sh
rustup target add wasm32-wasip2
WASI_SDK_PATH=/path/to/wasi-sdk sh pishoo/examples/journal/build.sh
cargo run --locked -p pishoo --example check-lib -- pishoo/examples/journal/dist/lib.wasm
DHTTP_TEST_OPENSSL=/path/to/openssl-3 cargo test --locked -p pishoo --lib execution::journal:: -- --include-ignored --skip journal_browser_preview --test-threads=1
```

构建生成 `dist/lib.wasm`，包含标准 wasi:http 导出与恰好一个 pishoo:openapi 清单。`dist/target` 不入版本控制，Cargo.lock 纳入版本控制。Rust 源码是普通 `src/journal.rs` 库根，HTML/CSS/JS 以内嵌页面形式打包。

真实组件测试通过 qtls 的完整内存 TLS 握手取得 RemoteAuthority，验证证书和 OCSP，不绕过构造边界。测试覆盖匿名和伪造头拒绝、同名不同 owner、作者伪造、跨站写入、发布/编辑/删除、留言/回复/删除、照片元数据清理、并发版本竞争、重新加载后持久化、Lib 目录隔离及 SQLite integrity_check。TLS roots 是进程全局资源，这组测试独立串行运行。

浏览器检查使用显式忽略的 `journal_browser_preview`：仅监听 127.0.0.1:18747，使用临时数据库和测试身份，全部页面与 API 由真实 Invocation 执行，五分钟后退出。它的 HTTP 适配仅用于界面验收，不作为生产认证入口。

```sh
DHTTP_TEST_OPENSSL=/path/to/openssl-3 cargo test --locked -p pishoo --lib journal_browser_preview -- --ignored --nocapture
```

照片测试样本来自本项目使用的 dyns 0.7.2 公共 `assets/ddns-logo.jpg`，仅用于验证 JPEG 处理。

2026-10-10 验收：WASM 编译/格式校验和 7 条 OpenAPI 路径校验通过，三个独立身份/真实组件专项及 29 项既有 Sandbox 回归通过；本机浏览器在真实 WASM 上完成照片发布、具名留言/回复、刷新持久化和 390px 手机布局检查。尚未安装到生产身份或改动其授权。

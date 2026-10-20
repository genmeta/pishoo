# 手记

记下生活里的小事，留住值得回看的瞬间。用文字和照片写手记，按月份翻阅往日记录，与朋友留言交流。简洁的阅读与写作界面，适合在手机和电脑上随时记录。

当前发布版本为 **0.1.0**，应用名称为「手记」，App 和 Lib 均使用完整 ID `alice.smith.dhttp.net/journal`，包类型分别为 `app` 和 `lib`。

## 项目组织

按 [App/Lib 软件包规范](../../docs/package-spec.md) 分别交付：

```text
journal/
├─ Cargo.toml / Cargo.lock
├─ src/journal.rs                   WASI HTTP 后端：记录、照片、留言和 SQLite
├─ public/index.html                手记页面
├─ public/assets/app.js / app.css   浏览器交互、桌面/手机布局和明暗模式
├─ openapi.json                     Lib 业务 API 与能力组
├─ wasmer.toml                      Lib 包清单：一个 backend，仅映射 metadata
├─ app.toml                         App 包清单：零 atom，仅映射 public
├─ lib.wasm                         自动生成的后端 component
├─ metadata/openapi.json            自动生成的 API 清单副本
└─ dist/
   ├─ journal-0.1.0-lib.webc         独立签名后端包
   └─ journal-0.1.0-app.webc         独立签名前端包，精确依赖同 ID 的 Lib 0.1.0
```

包身份、类型、展示信息、依赖、更新说明和宿主能力用途来自各自的签名 WEBC manifest；OpenAPI 只包含业务 API、请求/响应 schema 和访问声明。App 不包含 WASM 或 OpenAPI；Lib 不包含前端资源。component 不嵌入 OpenAPI 或签名。

入口为 `https://<安装者身份>/std/journal/`。HTML 使用 `./assets/...`，API 和照片使用相对 URL，解析到 `/std/journal/context`、`/std/journal/entries` 等路径。Lib 声明 `/journal/...`，宿主仅添加 `/std`；静态文件与业务操作共存。旧 `/api/journal/` 不提供别名。

## 构建与打包

准备 Rust 的 `wasm32-wasip2` 目标、`wasm-tools`、Python 3.11+ 和官方 WASI SDK。身份导入复用仓库的 `pishoo-sdk`（`../../../wit`），独立 SDK 发行包中为 `../../sdk`；SDK 尚未发布到 crates.io。

只构建后端与清单副本：

```sh
rustup target add wasm32-wasip2
WASI_SDK_PATH=/path/to/wasi-sdk sh pishoo/examples/journal/build.sh
wasm-tools component wit pishoo/examples/journal/lib.wasm
```

构建、签名并检查两个发布包：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh pishoo/examples/journal/build.sh \
  --publisher alice.smith.dhttp.net
```

构建脚本核对 Cargo、两个包清单、OpenAPI 的名称/版本和精确依赖，再通过 `genmeta build --no-build` 使用发布者本机 DHTTP 凭据分别签名并复用宿主校验。可设置 `GENMETA=/绝对路径/genmeta` 使用已编译工具；独立 SDK 发行包需要这个入口。签名证书必须与上传工作台实际加载的发布者证书一致。脚本拒绝覆盖已有同版本产物，发布内容变化必须递增版本并更新精确依赖。

也可用支持 `genmeta build --no-build` 的 gmutils 分别打包：

```sh
genmeta build --no-build pishoo/examples/journal/wasmer.toml --id alice.smith.dhttp.net \
  -o pishoo/examples/journal/dist/journal-0.1.0-lib.webc
genmeta build --no-build pishoo/examples/journal/app.toml --id alice.smith.dhttp.net \
  -o pishoo/examples/journal/dist/journal-0.1.0-app.webc
```

在 `alice.smith.dhttp.net` 工作台的商店上传页，**先发布 Lib，再发布 App**。商店会核对 App 依赖的 Lib 0.1.0 已发布，不能只上传前端。通过商店安装 App 时递归安装精确后端依赖；本地文件安装需先安装 Lib。在线安装后立即可用，离线 CLI 安装在下次启动加载。上传/安装均沿用现有原生 API，不添加自动授权。

## 身份与访问

页面、脚本、样式、照片和 API 都经过宿主 daccess。`public` 是资源目录名，不表示匿名公开。

Lib 显式声明以下业务能力组，安装时由宿主登记到 `config.db`，原生 `GET /std/acl/promissions` 提供只读目录：

| 权限组名称 | 业务操作 |
| --- | --- |
| 阅读 | GET context、entries、entry、photo、comments |
| 评论 | POST comments（留言及回复） |

以上路径均位于 `/std/journal/`。评论组不隐含阅读组；希望访客阅读并留言时，应同时授予两组相应操作。主人新建、修改、删除手记以及删除留言不属于访客能力组，继续校验真实主人身份。App 静态资源不登记为 Lib 能力，请另外配置 `/std/journal/` 和两个 assets 的 GET/HEAD 访问规则。能力声明和安装不会自动授权，也不会改写旧路径 ACL。

业务 API 通过 `pishoo:identity/request-identity@0.1.0` 读取已验证握手身份，拒绝匿名访问。发布、编辑、删除手记和删除留言还要求主人身份；主人检查同时核对名称与 owner_hash。留言署名来自可信身份，JSON 中伪造作者字段会被拒绝。留言验证标识表示发布时的认证，不表示历史证书现在仍有效。第一版没有逐篇访问范围开关，留言由主人管理。

写入要求同源 JSON 和 `x-journal-write: 1`；拒绝跨站标记、重复或不匹配 Origin，支持已验证 DHTTP 名称的 `~` 简写。请求头只用于浏览器跨站写入防护，不用于认证。正文与留言按文字显示，不执行上传的 HTML。页面不加载外部脚本、字体或跟踪服务，使用同源资源与 no-referrer 策略。静态响应由宿主提供 no-store/nosniff；业务响应继续提供既有安全头。

浏览器把 JPEG/PNG/WebP 转成最长边 1600 像素的 JPEG。后端只接受 1 MiB 以内、最长边 2048 的 JPEG，并剥离全部 APPn/COM 元数据；SVG 和外部图片 URL 不接受。每篇支持一张照片。

## 接口与数据

| 方法 | Lib 内部路径（宿主添加 `/std`） | 用途 |
| --- | --- | --- |
| GET | `/journal/context` | 当前可信身份与主人判断 |
| GET | `/journal/entries?offset=0` | 每页 20 篇，倒序翻阅 |
| POST | `/journal/entries` | 主人新建手记 |
| GET/PUT/DELETE | `/journal/entry?id=…` | 读取、主人编辑/删除 |
| GET | `/journal/photo?id=…` | 记录照片 |
| GET | `/journal/comments?entry_id=…` | 留言与一层回复 |
| POST | `/journal/comments` | 具名身份留言或回复 |
| DELETE | `/journal/comment?id=…` | 主人删除留言 |

每条新内容使用提交前生成的 16 随机字节十六进制 ID。重复创建返回 409，不覆盖既有记录；编辑/删除要求强 ETag 与 If-Match，缺少版本返回 428，旧版本返回 412。写入失败保留草稿，不把网络中断解释成未提交。

正文最多 64 KiB、标题 200 字符、留言 8 KiB，每篇最多 500 条留言/回复，请求体最多 1.5 MiB。回复只能指向同篇未删除的顶层留言。删除留言清空作者与正文、保留墓碑；删除手记清空记录和照片并删除留言，保留 ID 防止旧请求复活。

数据为身份目录下 `db/alice.smith.dhttp.net/journal/journal.db`，guest 只获得本 Lib 的 `/db`。沿用 schema v1、SQLite unix-dotfile VFS、rollback journal、synchronous=FULL 和即时事务。锁忙两秒返回 503；损坏、未知或不支持的数据库拒绝操作。更新同一完整 ID 时保留数据库。旧短 ID `journal` 的目录独立存在，不自动迁移或删除。

异常退出可能遗留 `.lock` 目录，只有确认服务停止后才能离线清理。备份需停服复制数据库及恢复文件，或使用 SQLite 一致快照；删除不自动改写已有备份。

## 验证与预览

真实组件测试使用 qtls 的证书/OCSP 验证握手，覆盖匿名与伪造身份、主人校验、跨站写入、记录/留言/照片、并发更新、持久化和数据库隔离。静态验收通过真实 WEBC 宿主与 daccess，检查 HTML/JS/CSS 的访问控制、GET/HEAD 和无 WASM 执行。

发布回归 `journal_release_registers_capabilities_and_pins_backend` 验证 App/Lib 独立内容、精确依赖、安装后的两组能力及六个业务操作、静态路由和卸载保护。

TLS roots 是进程全局资源，每个忽略测试应单独执行：

```sh
DHTTP_TEST_OPENSSL=/path/to/openssl-3 cargo test --locked -p pishoo --lib \
  journal_frontend_is_served_from_package_with_daccess_for_every_file -- --ignored
```

浏览器预览使用临时数据库、测试主人和真实 WEBC/WASM，仅监听本机，五分钟后退出：

```sh
DHTTP_TEST_OPENSSL=/path/to/openssl-3 cargo test --locked -p pishoo --lib \
  journal_browser_preview -- --ignored --nocapture
# http://127.0.0.1:18747/std/journal/
```

此 HTTP 适配仅用于界面检查，不作为生产身份认证入口。照片测试样本来自 dyns 0.7.2 的 `assets/ddns-logo.jpg`。

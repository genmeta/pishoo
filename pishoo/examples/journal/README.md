# 手记

记下生活里的小事，留住值得回看的瞬间。用文字和照片写手记，按月份翻阅往日记录，与朋友留言交流。简洁的阅读与写作界面，适合在手机和电脑上随时记录。

首个发布版本为 **0.1.0**，应用名称为「手记」，发布 ID 为 `alice.smith.dhttp.net/journal`。

## 项目组织

按 [Pishoo WASM 开发指南](../../docs/developing-wasm.md) 分离前端与后端：

```text
journal/
├─ Cargo.toml / Cargo.lock
├─ src/journal.rs            WASI HTTP 后端：记录、照片、留言和 SQLite
├─ public/
│  ├─ index.html             手记页面
│  └─ assets/
│     ├─ app.js              浏览器交互
│     └─ app.css             桌面/手机布局和明暗模式
├─ openapi.json              唯一维护的 HTTP 与权限清单
├─ wasmer.toml               backend atom 与 public/metadata 文件映射
├─ lib.wasm                  自动生成的后端 component
├─ metadata/openapi.json     自动生成的清单副本
└─ dist/journal-0.1.0.webc    签名发布包
```

页面、脚本和样式由 Pishoo 从 WEBC 文件卷读取，不执行 WASM。后端只处理业务 API；component 不嵌入页面、OpenAPI 或发布者签名。包包含一个 `backend` atom、`public`/`metadata` 文件卷和未映射的 `pishoo-signature` 签名卷。整个版本统一签名。

HTML 使用 `./assets/...`，API 和照片使用相对 URL，不写宿主挂载前缀。入口为 `https://<安装者身份>/std/api/alice.smith.dhttp.net/journal/`。OpenAPI 对三个静态文件逐个声明 GET/HEAD，与读取 API 一起归入阅读组；评论组包含留言/回复操作。组和清单不会自动授予访问权。

## 构建与打包

准备 Rust 的 `wasm32-wasip2` 目标、`wasm-tools`、Python 3.11+ 和官方 WASI SDK。身份导入复用仓库的 `pishoo-sdk`（`../../../wit`），独立 SDK 发行包中为 `../../sdk`；SDK 尚未发布到 crates.io。

只构建后端与清单副本：

```sh
rustup target add wasm32-wasip2
WASI_SDK_PATH=/path/to/wasi-sdk sh pishoo/examples/journal/build.sh
wasm-tools component wit pishoo/examples/journal/lib.wasm
```

构建、签名并检查完整发布包：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh pishoo/examples/journal/build.sh \
  --publisher alice.smith.dhttp.net
cargo run --locked -p pishoo --example check-lib -- \
  pishoo/examples/journal/dist/journal-0.1.0.webc
```

构建脚本核对 Cargo、wasmer.toml 和 OpenAPI 中的名称与版本。打包调用仓库的 `package-webc` 工具，通过发布者实际 DHTTP 身份加载凭据，在本机签名并复用宿主校验；不会把私钥或数据库写入发布包。可设置 `PISHOO_PACKAGE_WEBC=/path/to/package-webc` 使用已编译工具；独立 SDK 发行包使用这个入口。签名证书必须与上传工作台实际加载的发布者证书一致。

也可以执行 `wasmer package build . --check` 和 `wasmer package build . --out dist/journal-unsigned.webc` 检查标准包结构。其输出未签名，不能直接上传商店或安装。

在 `alice.smith.dhttp.net` 工作台的「应用商店 → 上传应用」中选择 `journal-0.1.0.webc`，检查后发布。发布内容改变时递增版本；同版本不同内容不能覆盖。通过工作台安装成功后即可打开；离线 CLI 安装在下次启动加载。

## 身份与访问

页面、脚本、样式、照片和 API 都经过宿主 daccess。请为需要访问的具名身份授权页面与两个 assets 的 GET/HEAD、context、记录、照片和留言读取，以及所需评论方法。`public` 是资源目录名，不表示匿名公开。

业务 API 通过 `pishoo:identity/request-identity@0.1.0` 读取已验证握手身份，拒绝匿名访问。发布、编辑、删除手记和删除留言还要求主人身份；主人检查同时核对名称与 owner_hash。留言署名来自可信身份，JSON 中伪造作者字段会被拒绝。留言验证标识表示发布时的认证，不表示历史证书现在仍有效。第一版没有逐篇访问范围开关，留言由主人管理。

写入要求同源 JSON 和 `x-journal-write: 1`；拒绝跨站标记、重复或不匹配 Origin，支持已验证 DHTTP 名称的 `~` 简写。请求头只用于浏览器跨站写入防护，不用于认证。正文与留言按文字显示，不执行上传的 HTML。页面不加载外部脚本、字体或跟踪服务，使用同源资源与 no-referrer 策略。静态响应由宿主提供 no-store/nosniff；业务响应继续提供既有安全头。

浏览器把 JPEG/PNG/WebP 转成最长边 1600 像素的 JPEG。后端只接受 1 MiB 以内、最长边 2048 的 JPEG，并剥离全部 APPn/COM 元数据；SVG 和外部图片 URL 不接受。每篇支持一张照片。

## 接口与数据

| 方法 | 内部路径 | 用途 |
| --- | --- | --- |
| GET/HEAD | `/`、`/assets/app.js`、`/assets/app.css` | 宿主读取页面与资源 |
| GET | `/context` | 当前可信身份与主人判断 |
| GET | `/entries?offset=0` | 每页 20 篇，倒序翻阅 |
| POST | `/entries` | 主人新建手记 |
| GET/PUT/DELETE | `/entry?id=…` | 读取、主人编辑/删除 |
| GET | `/photo?id=…` | 记录照片 |
| GET | `/comments?entry_id=…` | 留言与一层回复 |
| POST | `/comments` | 具名身份留言或回复 |
| DELETE | `/comment?id=…` | 主人删除留言 |

每条新内容使用提交前生成的 16 随机字节十六进制 ID。重复创建返回 409，不覆盖既有记录；编辑/删除要求强 ETag 与 If-Match，缺少版本返回 428，旧版本返回 412。写入失败保留草稿，不把网络中断解释成未提交。

正文最多 64 KiB、标题 200 字符、留言 8 KiB，每篇最多 500 条留言/回复，请求体最多 1.5 MiB。回复只能指向同篇未删除的顶层留言。删除留言清空作者与正文、保留墓碑；删除手记清空记录和照片并删除留言，保留 ID 防止旧请求复活。

数据为身份目录下 `db/alice.smith.dhttp.net/journal/journal.db`，guest 只获得本 Lib 的 `/db`。沿用 schema v1、SQLite unix-dotfile VFS、rollback journal、synchronous=FULL 和即时事务。锁忙两秒返回 503；损坏、未知或不支持的数据库拒绝操作。更新同一完整 ID 时保留数据库。旧短 ID `journal` 的目录独立存在，不自动迁移或删除。

异常退出可能遗留 `.lock` 目录，只有确认服务停止后才能离线清理。备份需停服复制数据库及恢复文件，或使用 SQLite 一致快照；删除不自动改写已有备份。

## 验证与预览

真实组件测试使用 qtls 的证书/OCSP 验证握手，覆盖匿名与伪造身份、主人校验、跨站写入、记录/留言/照片、并发更新、持久化和数据库隔离。静态验收通过真实 WEBC 宿主与 daccess，检查 HTML/JS/CSS 的访问控制、GET/HEAD 和无 WASM 执行。

TLS roots 是进程全局资源，每个忽略测试应单独执行：

```sh
DHTTP_TEST_OPENSSL=/path/to/openssl-3 cargo test --locked -p pishoo --lib \
  journal_frontend_is_served_from_package_with_daccess_for_every_file -- --ignored
```

浏览器预览使用临时数据库、测试主人和真实 WEBC/WASM，仅监听本机，五分钟后退出：

```sh
DHTTP_TEST_OPENSSL=/path/to/openssl-3 cargo test --locked -p pishoo --lib \
  journal_browser_preview -- --ignored --nocapture
# http://127.0.0.1:18747/std/api/journal/
```

此 HTTP 适配仅用于界面检查，不作为生产身份认证入口。照片测试样本来自 dyns 0.7.2 的 `assets/ddns-logo.jpg`。

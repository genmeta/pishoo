# Pishoo App / Lib 开发指南

日期：2026-10-14。本文按 [App/Lib 软件包规范](package-spec.md) 调整。本仓打包器、宿主与 Workspace/Chat 已适配签名 WEBC manifest；独立商店服务须支持同一格式后才能发布安装，尚未部署本次改动。旧合包不自动迁移。

## 文件职责

| 内容 | 职责 |
| --- | --- |
| WEBC manifest | 唯一包清单：标准 wapm/fs 注解，以及 pishoo 类型、依赖、入口、能力用途和更新说明 |
| `lib.wasm` | 标准 WASI HTTP backend Component，不能是 core module |
| Lib 的 `/metadata/openapi.json` | 唯一业务 API 描述，不承载安装元数据 |
| App 的 `/public/` | 浏览器构建产物，必须含 `index.html`，不需要 OpenAPI |
| `lib.webc` / `app.webc` | 两个独立版本、独立签名和独立发布的制品 |

包字段的精确位置、必填要求和完整包注解示例以 [软件包规范](package-spec.md#2-唯一包清单) 为准。开发输入可用 `wasmer.toml`，最终必须生成标准 WEBC manifest，不增加第二份权威清单。

## 项目和构建依赖

后端和前端分别组织，例如：

```text
hello/
├─ backend/
│  ├─ Cargo.toml
│  ├─ Cargo.lock
│  ├─ src/hello.rs
│  ├─ openapi.json
│  └─ lib.wasm
├─ frontend/
│  ├─ package.json
│  └─ public/
│     ├─ index.html
│     └─ assets/app.js
└─ dist/
   ├─ lib.webc
   └─ app.webc
```

Rust 库根使用与 crate 同名的 `.rs` 文件及 Cargo `[lib].path`，不使用 `lib.rs` 或 `mod.rs`。最小后端可用 `wasip2 = "=1.0.4"`、`serde_json = "1"`，库类型为 `cdylib`。保持锁文件并构建：

```sh
rustup target add wasm32-wasip2
cargo build --locked --release --target wasm32-wasip2
wasm-tools validate target/wasm32-wasip2/release/hello.wasm
wasm-tools component wit target/wasm32-wasip2/release/hello.wasm
```

Cargo/npm/Bun 依赖是构建依赖，保留在各自构建系统。运行时所需后端另写入 WEBC `package.pishoo.dependencies`；两者不混用。

## 编写 HTTP 后端

后端导出标准 `wasi:http/incoming-handler`，处理组件内部的 `/hello` 路径。宿主挂载前缀由 Pishoo 处理，代码不拼接发布者名称或 `/std/api`。

把下面的代码放进 `src/hello.rs`：

```rust
use std::io::Write;

use wasip2::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse,
    ResponseOutparam,
};

struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let method = request.method();
        let head = matches!(&method, Method::Head);
        let target = request.path_with_query().unwrap_or_else(|| "/".into());
        let path = target.split('?').next().unwrap_or("/");

        let (status, document) = if path != "/hello" {
            (404, serde_json::json!({"error": "not_found"}))
        } else if !matches!(&method, Method::Get | Method::Head) {
            (405, serde_json::json!({"error": "method_not_allowed"}))
        } else {
            (200, serde_json::json!({"message": "Hello from Pishoo WASM"}))
        };

        let bytes = serde_json::to_vec(&document).expect("serialize response");
        let fields = Fields::new();
        fields.set("content-type", &[b"application/json".to_vec()])
            .expect("set content type");
        fields.set("cache-control", &[b"no-store".to_vec()])
            .expect("set cache control");
        if status == 405 {
            fields.set("allow", &[b"GET, HEAD".to_vec()])
                .expect("set allowed methods");
        }

        let response = OutgoingResponse::new(fields);
        response.set_status_code(status).expect("set status");
        let body = response.body().expect("create body");
        ResponseOutparam::set(out, Ok(response));
        if !head {
            body.write().expect("open output stream")
                .write_all(&bytes).expect("write response");
        }
        OutgoingBody::finish(body, None).expect("finish response");
    }
}

wasip2::http::proxy::export!(Handler);
```

先交付响应，再写入并结束响应 Body。HEAD 使用相同状态和头，不写正文。最小示例用 `expect` 展示资源生命周期；实际业务应将可预期的输入、存储和业务错误映射为明确的 HTTP 响应，读写错误通过现有流的返回结果处理。

Pishoo 会按清单挂载方法和路径；handler 仍应按自身业务检查输入。不要把用户在请求头、query 或 JSON 中填写的身份当作可信来访者。

## 后端 OpenAPI

Lib 保留 OpenAPI 3.1，`info.version` 与包版本一致。只写组件内部业务路径，例如 `/hello`，每个操作提供唯一 operationId、非空 summary、真实参数与响应。固定路径不能包含宿主前缀、query 或路径模板；实际请求仍可携带已描述的 query 参数。

最小业务描述示例：

```json
{
  "openapi": "3.1.0",
  "info": {
    "title": "问候 API",
    "version": "1.0.0",
    "description": "提供问候接口。",
    "license": {"name": "MIT"}
  },
  "paths": {
    "/hello": {
      "get": {
        "operationId": "getHello",
        "summary": "读取问候语",
        "responses": {"200": {"description": "问候语 JSON"}}
      }
    }
  }
}
```

需要时继续使用已有 API 访问组和初始授权建议；建议不会自动写入 daccess。OpenAPI 不再写 publisher、name、kind、dependencies、entry、host、changelog 或 static-file。这些包字段由 manifest 负责，不能从 OpenAPI 回退读取。

## 独立前端

前端可使用 HTML/JS 或 Vite、React、Vue 等构建工具，最终生成 `public/index.html` 和资源文件。Vite `base` 使用 `./`，页面用 `./assets/app.js` 等相对资源 URL，业务调用则使用声明依赖对应的 `/api/<name>/...`，例如：

```javascript
const response = await fetch('/api/hello/hello', {
  headers: { Accept: 'application/json' },
})
```

App 安装于 `/app/hello/`；它与 `/api/hello/` 是独立挂载。资源目录直接生成静态 GET/HEAD 路由，不生成 OpenAPI、operationId 或 static-file 注解。不存在的资源404，不支持的方法405；SPA 使用 hash 路由，没有隐式 index.html fallback。所有资源仍经过 daccess，public 不代表匿名公开。

## 打包、签名和发布

1. Lib 写入一个 `backend` atom，只映射 `/metadata`，包含唯一 OpenAPI；App 零 atom，只映射 `/public`。
2. 两种包都填写 manifest 的 wapm/pishoo 注解和显式 dependencies；Lib 填写 host 用途，App 填写 entry=/。
3. 构建工具核对包身份、版本、实际内容、路径、imports/exports 和文件限额；整包签名保护 manifest、atoms 及内容卷。
4. 先发布依赖，再上传使用方。上传地址分别为 `/store/v1/libs/<publisher>/<name>/versions/<version>` 和 `/store/v1/apps/<publisher>/<name>/versions/<version>`，Content-Type 都是 `application/webc`。

同一类型、发布者、名称、版本不可覆盖。修改前端或依赖只递增 App 自身版本；修改后端递增 Lib 版本，需要使用新后端的使用方再更新自身依赖。签名算法和限额见 [商店协议](../../../store-server/docs/webc-protocol.md)。旧测试包从源码重新构建，不拆包、改签或迁移历史记录。

当前打包工具尚未实现上述 manifest 输入；实现完成前不要通过手改旧签名包或伪造 App OpenAPI 生成所谓新格式。

## 安装和卸载

商店安装继续调用 `POST /std/pishoo/store-install`，例如：

```json
{"kind":"app","publisher":"alice.smith.dhttp.net","name":"hello","version":"1.0.0"}
```

kind 选择 app/lib；省略时仍按 lib 处理。两种包都递归解析精确 Lib 依赖，复用匹配版本，补装缺失版本；循环、版本冲突、同类短名冲突或代理范围冲突时拒绝。缺少依赖的本地文件安装不自动联网补装。

安装事实以 config.db 为准，启动不扫描导入未登记文件。完整包身份用于签名、依赖、管理地址和磁盘，普通访问使用 `/app/<name>/`、`/api/<name>/`。在线操作即时生效，离线 CLI 修改下次加载生效；单制品原子保存，不承诺跨包事务。

卸载 App 保留后端；被 App/Lib 依赖的 Lib 不允许单独卸载或不兼容更新。显式卸载也保留后端数据和既有权限。不增加自动清理、级联删除或 purge；不自动授予页面/API 权限。

## 身份和私有数据

有权限的 HTTP 请求进入后端后，记录级权限仍由业务代码处理。例如发布或删除私有记录时，应验证可信来访者是否为主人，再执行存储操作。

当前 SDK 提供已实现的身份导入：

```toml
[dependencies]
pishoo-sdk = { version = "=0.1.0-preview.1", path = "../sdk" }
```

上面的 path 假设开发 SDK 的 `sdk/` 与项目目录相邻。SDK 当前从源码或开发者发行包获取，尚未发布到 crates.io；绑定说明见 [SDK 文档](sdk.md)。

```rust
use pishoo_sdk::request_identity;

let local = request_identity::local_name();
let caller = request_identity::caller_name();
let owner = request_identity::caller_is_owner();
```

这些值来自宿主认证信息。匿名 caller 为 None；主人判断同时核对名称和 owner_hash。使用该导入时，在 WEBC `package.pishoo.host` 中说明 request-identity 的具体用途；使用身份签名时说明 signatures 的用途。

持久数据写入当前 Lib 的 `/db`，对应身份目录下 `db/<publisher>/<name>/`。使用文件系统导入时，在 WEBC `package.pishoo.host` 中说明 private-storage 的用途。业务数据不进入 public、metadata 或发布包，卸载不自动清除数据与访问规则。

Lib 不获得通用宿主目录或命令执行能力。已批准的标准 WASI HTTP 出站仍经过既有宿主和 daccess，不由依赖或 host 用途声明授予权限；App 的 public 文件不作为 Lib 的预打开目录。

## 实施和验收边界

当前后端 Component 编译、独立 App/Lib 发布和基本安装已经存在；本次清单调整尚未实现。后续需要同时适配打包器、包校验、宿主安装/静态分派、商店元数据索引与前端展示，验证签名篡改、身份一致性、依赖解析、卸载保护及真实 daccess 行为后才能发布新版。

静态开发服务器只验证页面构建，不能证明实际宿主、身份、权限和依赖安装可用。新规范的字段和行为以 [package-spec.md](package-spec.md) 为准，生产服务版本以商店 DEPLOYMENT.md 为准。

App 与 Lib 均可声明多个精确后端 dependencies（publisher/name/version，最多100；Lib省略时为[]）。config.db 的 app_dependencies 与 lib_dependencies 分别保存关联。宿主拒绝循环和不匹配版本，启动核对签名制品与登记；依赖被使用时不能卸载或修改其内容/版本。正常移除父制品只删除其关联，保留依赖制品和数据。商店安装递归补齐并按依赖顺序提交，一次最多解析100个Lib，父安装失败不撤销已成功安装的有效依赖。

Lib 间调用通过标准 WASI HTTP 向 https://~/api/<name>/... 发请求。前台保留当前请求的真实调用者，继续受原生daccess和被调用Lib自身身份检查约束；后台CLI发起真实具名HTTP请求，调用身份由握手给出，CLI自身仍无caller。未声明依赖不会赋予额外权限，dependencies用于安装和生命周期约束。

前端详情展示所依赖后端及版本，静态文件GET/HEAD仅用于机器分派，不作为前端业务API展示。

## 本仓打包入口

```sh
cargo run --locked -p pishoo --example package-webc -- WASMER.toml OUTPUT.webc --publisher NAME
```

`wasmer.toml` 包含 `[package]` 的完整 name/version/description/license、`[package.pishoo]` 的 kind/title/dependencies/changelog 与 App entry 或 Lib host；`[fs]` 仅映射 App `/public` 或 Lib `/metadata`。Lib 的唯一 `[[module]]` 必须名为 backend，source 指向不嵌入 OpenAPI/签名的 Component。输入路径相对 TOML 文件解析。打包器加载发布者实际凭据、包级签名、调用宿主检查后原子写入输出，不生成 App OpenAPI 或改写源清单。

Workspace 的构建与三包依赖顺序见 [Workspace 后端](../workspace-lib/README.md)。

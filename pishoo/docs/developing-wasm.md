# Pishoo WASM 开发指南

使用标准 WASI HTTP component 编写后端，将页面、JavaScript、CSS 和图片构建到 `public/`，再把后端、前端和 OpenAPI 一起打包成标准 WEBC 制品。开发者交付一个 `.webc` 文件，Pishoo 负责验证发布来源、读取页面资源和执行后端。

本指南采用 WEBC 作为唯一发布格式，不要求兼容旧单 WASM 发布方式。一个发布包包含一个后端 component，对应一个 Lib；前后端共享发布者、名称和版本。

**实施状态：Pishoo 宿主已接入签名 WEBC v3 安装、整包校验及原生页面分派；独立 store-server 源码也已接入 WEBC 协议。** 下文的 Rust 后端和静态页面独立构建，`package-webc` 生成最终静态操作并签名，Workspace 上传和原生安装使用完整包。商店生产服务是否已切换以其部署记录为准；普通 `wasmer package build` 输出是未签名制品，不能直接上传或安装。正式协议见 [WEBC 商店协议](../../../store-server/docs/webc-protocol.md)。

## 文件各自负责什么

| 文件 | 职责 |
| --- | --- |
| `lib.wasm` | 标准 WASI HTTP 后端 component |
| `public/` | 可供浏览器读取的构建产物，包括 HTML、JS、CSS 和图片 |
| `openapi.json` | HTTP 接口、页面入口、访问分组和宿主能力用途的唯一权威清单 |
| `wasmer.toml` | 开发时的 package manifest，描述包、代码模块和文件映射 |
| `metadata/openapi.json` | 打包时生成的清单副本，随元数据文件卷进入 WEBC |
| `hello.webc` | 完整发布制品，包含后端、文件卷和包元数据 |

开发时编辑项目根的 `openapi.json`。打包时复制到 `metadata/openapi.json`，包内以映射后的 `/metadata/openapi.json` 为权威来源。不要手工维护两个不同的清单，也不在后端 component 中再嵌入一份。

WEBC 内部有 manifest、atoms 和 volumes。后端放在 atom，页面放在文件卷；下面的路径描述逻辑文件映射，不表示 WEBC 是 ZIP。这个组织方式沿用 [Wasmer 的 public 文件映射](https://docs.wasmer.io/edge/guides/cdn/)。

## Manifest 和 OpenAPI

`wasmer.toml` 就是开发时编写的包清单。Wasmer 打包工具将它转换成 WEBC 内置的 manifest；安装器从标准容器结构读取这份 manifest。项目不需要另写一份同义的 `manifest.json`。

两份清单分别表达不同的内容：

| 清单 | 描述什么 |
| --- | --- |
| WEBC manifest | 包名称、版本、模块，以及文件卷映射等包结构信息 |
| OpenAPI | HTTP 方法与路径、输入输出、页面入口、访问分组和宿主能力用途 |

WASI HTTP 的页面入口与 WEBC 的命令入口也属于不同层次。本指南的后端是 HTTP component，宿主调用 incoming-handler；不把页面入口写成一个 WASI CLI 命令。

发布制品的逻辑结构如下：

```text
hello.webc
├─ manifest                 由 wasmer.toml 转换生成的包清单
├─ atoms/
│  └─ backend               lib.wasm 的 component 字节
└─ volumes/
   ├─ public/
   │  ├─ index.html
   │  └─ assets/app.js
   └─ metadata/
      └─ openapi.json        独立 HTTP 清单
```

`/public` 与 `/metadata` 是文件映射后的虚拟位置，读取时依据 manifest 解析实际文件卷。包名称、版本和 OpenAPI 中的发布身份必须保持一致；构建工具应检查重复出现的值，发现矛盾就拒绝发布。包级签名应绑定 manifest、后端 atom 和全部文件卷，不能只保护 HTTP 清单。

## 创建项目

以一个返回问候语的 Lib 为例：

```text
hello/
├─ Cargo.toml
├─ Cargo.lock
├─ src/
│  └─ hello.rs
├─ openapi.json
├─ wasmer.toml
├─ public/
│  ├─ index.html
│  └─ assets/
│     └─ app.js
├─ metadata/                 打包时生成
│  └─ openapi.json
├─ lib.wasm                  后端构建产物
└─ dist/                     WEBC 构建产物
```

准备 Rust、`wasm-tools`，以及提供 `wasmer package build` 的 Wasmer CLI。为 Rust 安装编译目标：

```sh
rustup target add wasm32-wasip2
```

`wasm32-wasip2` 的输出已经是 component。普通 `wasm32-unknown-unknown` 输出不能直接作为本指南的后端。

在 `Cargo.toml` 中声明：

```toml
[package]
name = "hello"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
path = "src/hello.rs"
crate-type = ["cdylib"]

[dependencies]
wasip2 = "=1.0.4"
serde_json = "1"
```

这个最小后端只使用标准 HTTP 接口，不需要 Pishoo 专用 SDK。需要读取可信身份或使用身份签名时，再添加相应 SDK 导入。

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

## 编写 OpenAPI

使用 OpenAPI 3.1，路径写组件内部的 `/hello`。包版本、Cargo 项目版本和 `info.version` 在这个例子中均为 `0.1.0`。

下面是**后端 API 清单**。页面和静态资源的宿主操作需要由 WEBC 打包工具补齐，见后面的页面路由约定。

```json
{
  "openapi": "3.1.0",
  "info": {
    "title": "问候示例",
    "version": "0.1.0",
    "description": "一个具有独立页面和 WASI HTTP 后端的 Pishoo Lib。",
    "license": { "name": "Apache-2.0" }
  },
  "x-pishoo": {
    "publisher": "alice.smith.dhttp.net",
    "name": "hello",
    "host": {},
    "changelog": "首次发布，提供问候接口，无既有数据迁移。"
  },
  "paths": {
    "/hello": {
      "get": {
        "operationId": "getHello",
        "summary": "读取问候语",
        "responses": {
          "200": {
            "description": "返回问候语",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "required": ["message"],
                  "properties": { "message": { "type": "string" } }
                }
              }
            }
          }
        }
      },
      "head": {
        "operationId": "headHello",
        "summary": "读取问候接口响应头",
        "responses": { "200": { "description": "不含正文的响应" } }
      }
    }
  }
}
```

用自己的规范化 DHTTP 名称替换 publisher；名称是发布身份，不是任意展示文字。Lib 名称以小写字母开头，后续使用小写字母、数字和连字符，最多 63 字符。版本使用无前导零的 `MAJOR.MINOR.PATCH`。

每个操作提供唯一 `operationId` 和非空 `summary`。使用固定路径，不带宿主前缀、query 字符串或 `{id}` 路径模板；query 参数可以在实际请求中传入。清单应描述真实的输入、输出和业务错误，示例之外的接口需要补齐相应 schema 与响应。

`host` 声明实际导入的宿主能力及其用途；本例不调用这些能力，因此为空。权限分组和初始授权建议可按既有 OpenAPI 发布格式声明，组成员须与操作注解一致且不重叠。清单中的建议不会自动变成访问规则。

## 编写独立前端

将下面的页面放进 `public/index.html`：

```html
<!doctype html>
<html lang="zh-CN">
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>问候示例</title>
  <h1>问候示例</h1>
  <button id="hello" type="button">请求后端</button>
  <p id="result" role="status"></p>
  <script type="module" src="./assets/app.js"></script>
</html>
```

把下面的代码放进 `public/assets/app.js`：

```javascript
const output = document.querySelector('#result')

document.querySelector('#hello').addEventListener('click', async () => {
  try {
    const response = await fetch('./hello', {
      headers: { Accept: 'application/json' },
    })
    if (!response.ok) throw new Error(`HTTP ${response.status}`)
    const result = await response.json()
    output.textContent = result.message
  } catch (error) {
    output.textContent = `请求失败：${error.message}`
  }
})
```

页面使用相对 URL。若 Lib 入口是 `/std/api/alice.smith.dhttp.net/hello/`，`./hello` 会请求 `/std/api/alice.smith.dhttp.net/hello/hello`；宿主将内部 `/hello` 交给 WASM。页面不写安装者身份、完整挂载前缀或外部 API 主机。

对于 Vite、React 或 Vue 项目，把源码放在独立的 `frontend/`，构建结果输出到 `public/`。Vite 的 `base` 使用 `./`，避免生成从宿主根目录开始的 `/assets/...`。生成的 HTML、JS、CSS 和图片全部随同一版本发布。

### 页面路由约定

宿主按最终 OpenAPI 读取 public 文件卷：

| 组件内 HTTP 路径 | 处理方式 |
| --- | --- |
| `/` | 宿主读取 `/public/index.html` |
| `/assets/app.js` | 宿主读取 `/public/assets/app.js` |
| `/hello` | 调用 WASM incoming-handler |

商店协议已规定机器映射：`index.html` 对应 `/`，其余 public 文件对应去掉 `/public` 的固定路径；每个文件都声明 GET 和 HEAD，操作的 `x-pishoo.static-file` 写映射后的 `/public/...` 文件路径。有 index.html 时入口 `x-pishoo.entry` 为 `/`。静态路径上不能同时声明后端操作。正式打包工具须将这些操作补入同一份最终 OpenAPI；上面的 API 示例不能被当作已经完整的可发布清单。宿主对静态 GET/HEAD 使用原生文件分派，其余操作进入 WASM；两者都经过 daccess。

前端文件与业务 API 路径冲突时，打包检查应失败。SPA 可以先使用 hash 路由；其他外部路径需要明确声明，不假设宿主把任意缺失路径回退到 `index.html`。

`public` 是资源目录名，不代表匿名可访问。页面、脚本、图片和业务 API 都经过 daccess。页面同源运行，也不代表浏览器已获得逐 Lib 隔离。

## 构建后端

在新项目目录执行：

```sh
cargo generate-lockfile
cargo build --locked --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/hello.wasm lib.wasm
wasm-tools validate lib.wasm
wasm-tools component wit lib.wasm
```

提交 `Cargo.lock` 并在后续构建中保留 `--locked`。检查输出是 component，包含标准 HTTP incoming-handler 导出；imports 必须能由 Pishoo 宿主满足。`wasm-tools validate` 校验二进制结构，宿主的链接与导出检查决定实际执行兼容性。

前端修改时可以复用已有 `lib.wasm` 业务代码，重新构建 public、更新发布版本并重新打包签名。后端有修改时再重新编译。HTTP 清单和代码的版本、路径或行为不一致时应先修正，再发布。

## 构建 WEBC

在项目根放置以下 `wasmer.toml`：

```toml
[package]
name = "alice.smith.dhttp.net/hello"
version = "0.1.0"
description = "Pishoo HTTP component with an independent frontend"
license = "Apache-2.0"

[[module]]
name = "backend"
source = "lib.wasm"

[fs]
"/public" = "public"
"/metadata" = "metadata"
```

这里选择一个名为 backend 的 atom，Pishoo 按标准 WASI HTTP 调用它。没有将它声明为一个 WASI 命令或依赖 `wasmer/static-web-server`：前端由 Pishoo 原生服务，后端执行仍交给 Pishoo 的 Wasmtime 宿主。包可被 WEBC 工具解析，与 component 能否在某个运行时执行，是两项分别检查的能力。

准备映射目录并使用 Wasmer 的标准打包命令：

```sh
mkdir -p metadata dist
cp openapi.json metadata/openapi.json
wasmer package build . --check
wasmer package build . --out dist/hello-0.1.0.webc
```

`metadata/openapi.json` 是自动准备的副本。`/public` 和 `/metadata` 是 manifest 中的虚拟文件映射，WEBC 实际包含相应文件卷；读取器需要遵循映射解析文件位置，不能把它当成普通 ZIP 目录。

打包命令会检查包能否序列化为 WEBC，但不替代 Pishoo 对 OpenAPI、WASI HTTP imports/exports、文件完整性、访问声明和发布身份的检查。它也不会自动生成本例尚缺的页面 HTTP 操作。

本示例沿用 [Wasmer 文件映射](https://docs.wasmer.io/edge/guides/cdn/)，命令参数以 [Wasmer package build 实现](https://github.com/wasmerio/wasmer/blob/main/lib/cli/src/commands/package/build.rs) 为依据。包读取结构见 [WEBC 库文档](https://docs.rs/webc/latest/webc/)。商店当前只接收 WEBC v3，固定使用 webc 12.0.1；其他 WEBC 版本拒绝。

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

这些值来自宿主认证信息。匿名 caller 为 None；主人判断同时核对名称和 owner_hash。使用该导入时，在 `host` 中说明 request-identity 的具体用途；使用身份签名时说明 signatures 的用途。

持久数据写入当前 Lib 的 `/db`，对应身份目录下 `db/<publisher>/<name>/`。使用文件系统导入时，在 host 中说明 private-storage 的用途。业务数据不进入 public、metadata 或发布包，卸载不自动清除数据与访问规则。

普通 Lib 不获得通用宿主目录、命令执行或默认 HTTP 出站能力。WEBC 的 public 文件映射也不意味着 WASM 自动获得该卷的读写权限。

## 预览和验收

可以先预览静态页面：

```sh
python3 -m http.server 5173 --bind 127.0.0.1 --directory public
```

这一步只验证 HTML、JS 和布局，`/hello` 尚无后端，会返回 404。完整 API、可信身份和访问规则需要在支持该 WEBC 协议的隔离 Pishoo 环境中验收；前端开发 mock 不作为真实身份或权限验收。

发布前依次确认：

1. 后端是有效 component，标准 incoming-handler 能被宿主链接。
2. OpenAPI 与实际方法、路径、响应和宿主能力一致。
3. public 中的全部构建依赖可读取，浏览器请求使用相对路径。
4. 最终包包含一个 backend atom 和唯一 OpenAPI，版本一致，文件完整。
5. 页面与 API 分别经过 daccess；主人操作和记录级检查在后端生效。

首次创建资源、并发更新、重复提交、输入错误和存储错误，应按具体业务补验收；示例的只读问候语不代表已验证这些业务行为。

## 签名、检查和安装

使用仓库的 `package-webc` 工具从独立后端、OpenAPI 和 public 目录构建完整签名包：

```sh
# 从 Pishoo 仓库执行，发布者须在本机 DHTTP_HOME 中持有有效凭据。
cargo run --locked -p pishoo --example package-webc -- \
  /path/to/hello/lib.wasm /path/to/hello/openapi.json \
  /path/to/hello/public /path/to/hello/dist/hello-0.1.0.webc \
  --publisher alice.smith.dhttp.net

pishoo lib check /path/to/hello/dist/hello-0.1.0.webc
pishoo lib --id installer.dhttp.net install alice.smith.dhttp.net/hello \
  /path/to/hello/dist/hello-0.1.0.webc
pishoo restart
```

工具核对项目 Cargo.toml、wasmer.toml 与 OpenAPI 的名称/版本，检查 backend 的宿主兼容性并生成每个 public 文件的 GET/HEAD。已正确声明的静态操作保留 operationId、summary、access-group；映射不符或后端路径冲突会拒绝。工具随后用发布者身份签名，通过同一宿主检查后，原子写出包与 `metadata/openapi.json`。私钥不进入发布包。`wasmer package build` 可用于标准 WEBC 格式检查，不能替代最终清单与 Pishoo 包级签名。

包级签名放入未映射的标准 `pishoo-signature` 卷，唯一文件为 `/signature.json`，覆盖原始 manifest、backend atoms 和全部内容卷，具体算法见 [WEBC 商店协议](../../../store-server/docs/webc-protocol.md)。历史包仍验证签名内容和发布者身份，证书今日过期不删除旧发布；在线商店来源另经固定商店身份及发布证明核对。

完整包最多64MiB，OpenAPI、WEBC manifest/index 各最多1MiB；卷内文件最多16MiB、条目最多4096、深度32、卷最多16。签名信封最多64KiB，签名最多8KiB。拒绝符号链接、未索引尾部字节、重叠段、外部卷依赖、多个后端 atom 和后端内重复 OpenAPI/签名。修改并重算 WEBC 内部摘要仍须通过发布者签名验证。

HTTP `POST /std/pishoo/lib-check` 和 `PUT /std/pishoo/libs/<publisher>/<name>` 使用 `Content-Type: application/webc` 和完整包字节。商店上传使用同一格式，固定版本下载路径为 `.../versions/<version>/lib.webc`。原生商店安装仍调用 `POST /std/pishoo/store-install`，由宿主重新下载、核对发布证明、整包验签和编译。

磁盘制品为 `<身份目录>/lib/<publisher>/<name>/lib.webc`，数据仍在独立 `/db`。Workspace 和 HTTP 管理 API 安装、更新后立即一起启用新版前后端，卸载后立即撤销新请求入口；已开始的请求保留旧 Lib 到自身结束。保存或候选准备失败保留原版。离线 CLI 只修改磁盘，其改动在下次启动加载。安装、更新和卸载不自动修改访问规则，卸载保留数据。旧 `lib.wasm` 不自动转换，须从项目重新构建 WEBC 并安装。

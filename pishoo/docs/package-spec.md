# App / Lib 发布包规范

日期：2026-10-20。本文定义 Pishoo 接受的发布制品格式、清单、依赖、OpenAPI 和签名字节。安装登记、HTTP API、目录归属与运行生命周期见[包管理](package-management.md)；构建、发布和管理命令见 [genmeta 包命令](genmeta-package.md)。

本文“源包”指原始已签发布制品，不指源代码工程。发布制品使用 WEBC v3。`genmeta build` 在本地构建并签名，不调用宿主预检查接口，也不需要运行中的 Pishoo。构建成功表示本地打包与签名完成；完整包校验、Lib 编译和宿主接口兼容检查发生在从商店实际安装或更新时，不能把离线构建结果当作宿主兼容认证。宿主只接收精确商店版本的 JSON PUT，不提供本地 WEBC 安装入口；build 和 publish 仍分别负责本地签名与商店发布。

## 制品类型和容器布局

App 是浏览器前端，Lib 是一个 WASI HTTP 后端 Component。两者独立发布和版本管理，使用相同的签名容器格式。

| 内容 | App | Lib |
| --- | --- | --- |
| 常用发布文件名 | `app.webc` | `lib.webc` |
| atoms | 空 | 恰好一个名为 `backend` 的 Component |
| 必需运行内容 | 映射后的 `/public/index.html` | `backend` 及其业务 OpenAPI |
| 文件系统映射 | 恰好一个本包 `/public` 映射 | 恰好一个本包 `/metadata` 映射，可以为空目录 |
| OpenAPI | 无后端 API 声明 | 新包嵌入 `backend` 顶层 `pishoo:openapi` custom section |
| 签名 | 未映射的 `pishoo-signature` 卷 | 未映射的 `pishoo-signature` 卷 |

输出文件可以采用其他文件名，例如 `message-0.1.0-lib.webc`；包身份来自清单。`.webc` 是携带元数据、内容和签名的发布容器，不能作为 WASM Component 直接执行。只有 Lib 的 `backend` atom 是 WASM；Pishoo 不使用 WEBC 的命令入口来运行它。

容器必须使用标准 v3 header `00 77 65 62 63 30 30 33`。manifest 的 atoms 声明及摘要须与实际 atom 一致；`backend` 必须是 Component，不能是 core module。禁止 WEBC `use-map`、commands、命令 entrypoint 和 bindings。

`package.fs` 的 `volume_name` 指向本包内容卷，`mount_path` 按类型固定为 `/public` 或 `/metadata`。卷名可以不同于挂载名，读取以映射为准；可选 `host_path` 指定卷内真实目录，省略时为 `/`。不允许 `from` 外部包映射，不允许把签名卷映射给 guest。除该内容卷和签名卷外，不接受额外卷。

下面是打包器的本地输入布局，不是把 WEBC 当成 ZIP 展开后的目录约定：

```text
App 工程/                  Lib 工程/
├── wasmer.toml            ├── wasmer.toml
└── public/                ├── lib.wasm       # 已构建的 Component，含内嵌 OpenAPI
    ├── index.html         └── metadata/      # 可以为空
    └── assets/...
```

Lib 构建输入也可以暂时提供 `metadata/openapi.json`。打包器在签名前将其嵌入 Component，最终内容卷不再包含该旁置文件；已有内嵌文档和构建输入旁置文档同时提供时，二者字节必须相同。构建配方、源码、Cargo/npm 依赖留在工程中，不是发布清单字段。

## 唯一发布清单

发布包内的 WEBC manifest 是该版本包元数据的唯一权威。沿用 `package.wapm`、`package.fs` 和具体的 `package.pishoo` 注解。实际清单按 WEBC 标准使用 CBOR 编码；文档中的 TOML/JSON 是构建输入或可读表示，不是另一份 `manifest.json`。

| 位置 | 字段 | 发布包要求 |
| --- | --- | --- |
| `package.wapm` | `name` | 必填，完整 `<publisher>/<name>` |
| `package.wapm` | `version` | 必填，固定 `MAJOR.MINOR.PATCH` |
| `package.wapm` | `description` | 必填，非空包说明 |
| `package.wapm` | `license` | 必填，非空许可证字符串；当前不另做 SPDX 表达式校验 |
| `package.wapm` | `repository` | 可选，非空绝对 HTTPS URL，须有主机，不含用户名、密码或空白 |
| `package.pishoo` | `kind` | 必填，`app` 或 `lib`，与实际内容及发布集合一致 |
| `package.pishoo` | `title` | 必填，非空展示名称 |
| `package.pishoo` | `dependencies` | 必填，精确 Lib 依赖数组；无依赖写 `[]` |
| `package.pishoo` | `changelog` | 必填，非空版本变化说明 |
| `package.pishoo` | `entry` | 仅 App 必填，固定 `/`；Lib 禁止声明 |
| `package.pishoo` | `host` | 仅 Lib 必填，宿主能力用途对象；无用途写 `{}`；App 禁止声明 |
| `package.fs` | 文件映射 | 按上一节映射一个本包内容目录 |

`package.pishoo` 只接受表中的字段，不接受额外字段、重复身份字段或任意扩展袋。Pishoo 从上述 `wapm` 字段读取包身份与展示信息；不要从 OpenAPI 补齐缺失清单字段。`repository` 保留原字符串并参与签名，只作为源码链接，不触发拉取、执行或身份认证。

`host` 只允许下列键，值均为非空用途说明：

| 键 | 对应 imports |
| --- | --- |
| `private-storage` | `wasi:filesystem/` |
| `request-identity` | `pishoo:identity/request-identity@` |
| `signatures` | `pishoo:identity/signatures@` |

安装时核对实际 imports：使用这些宿主接口必须说明用途。用途文字不授予权限，也不扩展宿主支持的接口。是否允许调用业务 API 仍由 daccess 决定。

最小 App 构建清单示例：

```toml
[package]
name = "alice.dhttp.net/echo"
version = "1.0.0"
description = "Echo 前端"
license = "MIT"

[package.pishoo]
kind = "app"
title = "Echo"
changelog = "首次发布前端。"
entry = "/"
dependencies = [
  { publisher = "alice.dhttp.net", name = "echo", version = "1.0.0" }
]

[fs]
"/public" = "public"
```

对应 Lib 构建清单示例：

```toml
[package]
name = "alice.dhttp.net/echo"
version = "1.0.0"
description = "Echo 后端"
license = "MIT"

[package.pishoo]
kind = "lib"
title = "Echo API"
changelog = "首次发布接口。"
dependencies = []
host = {}

[[module]]
name = "backend"
source = "lib.wasm"

[fs]
"/metadata" = "metadata"
```

`[build]` 等本地构建配方不会写入发布清单，具体命令和输出路径规则见 [genmeta 包命令](genmeta-package.md)。

## 身份、版本和精确依赖

发布身份是 `(kind, publisher, name)`。`publisher` 必须是规范化的完整 DHTTP 名称，例如 `alice.dhttp.net`；短作者输入仅是 CLI 简写，不写入签名包。清单不使用 `@作者/模块`。

短名最长63字节，以小写 ASCII 字母开头，只含小写字母、数字和连字符。Pishoo 安装保留短名 `std`。App 和 Lib 是不同类型，可以使用相同作者、短名和版本；发布者不同的同类包也有不同完整身份。本机同一类型的短名跨作者唯一：不能同时安装 alice/echo 与 bob/echo 的两个 Lib，但可安装同名 App 和 Lib。作者保存在元数据中，不用于新建文件或业务目录名称；详细规则见[包管理](package-management.md)。

版本恰好三段，每段为可表示为 `u64` 的非负十进制整数；除 `0` 外不带前导零。不接受 prerelease、build metadata、版本范围或 `latest` 作为清单版本。商店的同身份同版本内容不可覆盖，内容变化应发布新版本。

`dependencies` 最多100项，每项恰好包含三个必填字符串：

```json
{"publisher":"alice.dhttp.net","name":"echo","version":"1.0.0"}
```

依赖目标总是 Lib，其作者、短名和版本遵循相同规范。同一完整 Lib ID 不得重复；Lib 不能直接依赖自身。App 可以依赖与自己同作者、同短名的 Lib，前后端类型不同。这里不定义可选依赖、版本范围、备选项、虚拟包或 WASM 模块链接。

依赖声明描述运行关系，不授予跨 Lib 调用权限。递归安装、版本冲突、循环检查及卸载保护由[包管理](package-management.md)规定。

## Lib 的 OpenAPI

新 Lib 将原始 OpenAPI UTF-8 JSON 嵌入 `backend` 顶层唯一的 `pishoo:openapi` custom section，内容不超过1 MiB。文档必须能按 OpenAPI 3.1.x 解析，包含 `paths` 和非空 `info.title`，`info.version` 必须等于包版本。

文档描述 Component 内部业务路径，例如 `/echo`。宿主添加 `/std` 前缀，清单身份和运行依赖不从该路径或 OpenAPI 推断。业务路径必须固定：不含 query、fragment、百分号、反斜杠、空白、`.`/`..` 路径段或 `{参数}` 模板；不占用宿主保留路径。路径级 `$ref` 不可提供实际路由方法，因此拒绝。

每个实际操作都要有非空 `operationId` 和 `summary`，`operationId` 在该 Lib 内唯一。标准 responses、schemas 等继续按 OpenAPI 描述，不把包身份、`kind`、`dependencies`、`entry`、`host`、`changelog` 或静态文件路由放进 `x-pishoo`。

Pishoo 的业务扩展为：文档级 `x-pishoo.access-groups`，操作级 `x-pishoo.access-group` 和 `x-pishoo.initial-access`。组名采用短名语法，组有非空 title 和非空 operations 列表；成员必须指向真实操作，与操作注解一致且不交叠。初始授权建议不能代替 daccess 授权。

App 没有后端 OpenAPI 声明。`public/` 是实际静态文件树，不需要为每个文件生成 `operationId`、`summary` 或 `static-file` 注解；运行时页面路径、深链接回退和授权规则见[包管理](package-management.md)。

### 历史旁置文档

宿主仍接受原有已签 Lib 中映射后的 `/metadata/openapi.json`，前提是 backend 没有内嵌 OpenAPI。顶层重复嵌入、嵌套层嵌入、嵌入与旁置文件同时存在均拒绝；双来源即使字节相同也拒绝。Component 任意嵌套层的 `pishoo:signature` 均禁止，发布签名只能位于包级签名卷。

这与构建输入不同：构建输入允许同字节的内嵌文档和临时旁置文档，因为打包器会移除最终卷中的旁置文档；已签发布输入必须只有一处真实来源。

从商店取得历史包进行安装时，先验证其原始签名，再把原 backend 字节加上一个最终的 `pishoo:openapi` section，作为安装后的运行 WASM。该追加只使用已验证的原始 OpenAPI 字节；原 WEBC 和它的签名不被改写，安装表不保存原 WEBC 摘要，也不以它核查后续运行文件。重新构建发布则生成新的签名制品。

## 包级签名字节

签名保存在没有 fs 映射的 `pishoo-signature` 卷中。该卷恰好只有根级普通文件 `/signature.json`，无子目录或其他文件。它不是 WEBC 内置的 `index.signature`；后者必须为 None/空值。

签名信封恰好包含三个字段：

```json
{
  "version": 1,
  "certificates": ["base64 DER leaf", "base64 DER intermediate"],
  "signature": "base64 signature bytes"
}
```

证书数组非空、叶证书在前；各项为非空 DER 证书的标准 Base64。签名也使用标准 Base64。实际身份校验核对叶证书 SAN 中的完整 publisher、DHTTP owner_hash 和对应公钥签名；在线商店访问还核对固定商店身份及发布证明。历史签名不会仅因签名证书今天到期而被直接否定。

签名载荷按以下顺序计算，使用原始二进制 section，包含各 section 自身的头部、路径和时间戳：

```text
payload_digest = SHA256(
    bytes("\0webc003")
    || raw_manifest_section
    || raw_atoms_section
    || raw_non_signature_volume_sections_in_volume_name_order
)

signed_input = bytes("pishoo-webc-signature-v1\0") || payload_digest
signature = Sign(publisher_private_key, signed_input)
```

卷按索引中的卷名排序，排除 `pishoo-signature`。索引本身及签名卷不属于签名载荷；安装前仍校验索引、section span、checksum、完整覆盖和卷文件摘要，不允许未登记尾部字节、重叠或缺口。不能把 JSON 重新序列化后代替原始 manifest CBOR 来计算签名。

当前签名密钥方案为 RSA-PSS/SHA-512、P-256 ECDSA/SHA-256、P-384 ECDSA/SHA-384、Ed25519。验签按证书公钥选择对应规范方案，不接受任意算法协商。普通 WEBC atom 摘要使用 `sha256:<Base64>`；商店发布/下载协议中的 `package_sha256` 则是**最终原始 WEBC 全部字节**的 SHA-256 十六进制值，与上述不包含签名卷的 `payload_digest` 用途不同。它参与源包传输校验，不是本机安装表字段。

修改清单、依赖、内嵌 OpenAPI、WASM、文件路径、文件内容或参与签名的时间戳都改变签名载荷，必须重新签名。

## 格式限额

MiB/KiB 分别指 `1024²` / `1024` 字节。以下是宿主发布输入检查的上限，本地打包也必须为最终容器开销预留空间。

| 项目 | 上限或规则 |
| --- | --- |
| 完整 WEBC | 64 MiB |
| index 区 | 结束偏移不超过1 MiB，包含前面的容器/index 头部 |
| manifest section | 完整 section 不超过1 MiB |
| OpenAPI 原始 JSON | 1 MiB |
| 内容卷内单个文件 | 16 MiB；`backend` atom 不属于该单文件限额，仍受整包上限约束 |
| 全部卷的文件/目录条目 | 累计4096项，包含签名文件；卷根不算条目 |
| 卷内目录深度 | 32，卷根深度为0 |
| 卷内绝对路径 | 1024个 UTF-8 字节；不含歧义路径段、控制/空白字符、问号、`#`、`%` 或反斜杠 |
| 卷数 | 通用解析上限16；本规范实际只接受内容卷和签名卷 |
| `/signature.json` | 64 KiB |
| 解码后的签名 | 1至8192字节 |
| 直接 Lib 依赖 | 最多100项，必须精确版本 |

卷内只接受常规文件和目录，拒绝符号链接、重复同级名称及其他特殊条目。路径不能有空段、`.`、`..`，根路径 `/` 之外不能以 `/` 结尾。

## 发布包与安装资源的区别

`changelog` 和 Lib `host` 用途是**发布清单必填内容**。安装数据库有意不保存这些文字，也不保存原始清单、签名、证书、proof、`package_sha256` 或 `author_owner_hash`；这不放宽新发布输入的要求。每类安装登记仅有 name、author、version、title、description、license、repository 七列。

安装后保留实际运行文件和数据库登记。Pishoo 可以用登记及运行文件构造临时无签名 Container，供已有 Lib/App 运行接口使用；它不是原始发布包，不能拿它还原原签名或与原 WEBC 全字节摘要比较。签名、来源和完整性只在实际安装/更新输入时核对，不持久化本地防篡改指纹，启动不审计安装后的文件内容。登记、校验时机及旧布局兼容规则见[包管理](package-management.md)。

源 App 的 `/public` 映射不因安装路径调整而改变。标准 App 与其他 App 安装后都使用 `apps/<name>/public/...`，没有 `apps/std` 分层；标准 Lib 仍有 `libs/std/<name>.wasm` 的物理归类。新业务数据目录为 `db/<name>/`；已有 `db/<author>/<name>/` 可兼容读取，但不新建作者目录或自动迁移。完整安装布局以[包管理](package-management.md)为准。

## 当前商店兼容边界

独立 `store-server` 本轮保持不变。其当前校验代码仍要求 Lib 的 `/metadata/openapi.json`，并拒绝 backend 内任何 `pishoo:openapi`。因此，新内嵌 OpenAPI Lib 虽可由新版工具构建，却不能经这个未适配商店完成发布和安装。安装/更新唯一走商店，不能用本地上传绕过该阻塞；须先另行完成商店兼容和部署。本轮不修改应用商店。

App 的内容布局和包级签名没有因 OpenAPI 内嵌而改变。商店目前的实际协议见[商店 WEBC 协议](../../../store-server/docs/webc-protocol.md)，部署状态以其部署记录为准。本文不宣称商店已经升级，也不触发发布、部署或数据迁移。

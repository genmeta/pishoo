# 身份级 WASM 沙盒与 OpenAPI

日期：2026-09-22。状态：设计提案，未实现。本文取代此前的“每 Lib 一个沙盒、component 导出私有 manifest”方案。主设计见 [整体设计](pishoo-wasm-db-redesign.md)，执行接缝见 [DHTTP 接入说明](dhttp-integration-contract.md)。

## 当前实现进度（2026-09-24）

Pishoo 的归属关系为 `Server → Sandbox → 多个 Lib`。`Server` 不再另存 Lib 列表；Lib 仅保存编译组件与文件能力，不持有 Sandbox。所有调用经 `Sandbox.handle(request, scheme, limits)` 按 HTTP method 和 path 匹配 OpenAPI 声明的路由并准入，底层执行入口仅 crate 内可见。外部路径为 `/api/<LibId><声明路径>`，query 不参与匹配，向 guest 转交时剥离挂载前缀并保留 query。未声明路径返回 `RouteNotFound`，已声明路径的不支持方法返回 `MethodNotAllowed`，由外层 HTTP 服务映射为 404/405。每次调用仍独立创建 Store/Instance；共享 Engine/Linker 不共享 guest 可变状态。

`Sandbox.load_deployments(profile)` 校验部署身份并加载含 OpenAPI 的 Lib；`load_lib(id, bytes)` 同样读取组件内嵌 OpenAPI 注册路由，不授予文件能力。编译失败保留旧版本，重载复用同一沙箱预算。`remove_lib(id)` 停止该 Lib 的新调用；已准入调用持有原版本直到完成或身份取消。此入口尚不实现自动磁盘删除检测和单 Lib 在途取消。

默认每身份最多 16 个在途请求、256 MiB 线性内存预留总额、4 亿在途 fuel 预留总额，请求 deadline 不超过 30 秒。调用者给出请求限额；准入时一次性预留，超限立即返回错误，不排队。单次请求的内存上限累计所有线性 memory；共享 memory 被禁用。fuel 是在途调用的 allowance 总和，不是按时间窗口计量的 CPU 配额。定期 fuel yield 保证 CPU 密集 guest 也能被取消。

预算由 guest 执行和响应 body 共同持有，二者均结束后归还。`cancel()` 永久关闭该沙箱准入并取消子请求；其他身份不受影响。调用方仍需消费或丢弃响应 body 才能释放其保留的资源。重新启用身份需创建新的沙箱。

部署加载时仅打开当前 `lib/<LibId>/data/`，固定其目录句柄；请求获得独立 WASI 上下文及该目录的读写能力，挂载点为 `/data`。不会预打开身份根目录、其他 Lib、file、ssl、db 或宿主环境，HTTP 出站由宿主注入的 `OutgoingContext` 与 `OutgoingHandler` 控制：可信调用者必须与沙箱身份一致，再由宿主按实际目标决定是否发送。缺少可信上下文、身份不一致或请求已取消时拒绝；不读取 guest header 作为身份。宿主处理器负责目标与管理路径策略、重定向复核、身份绑定和 I/O 取消，当前使用 Mock 宿主验证，尚未接 DHTTP 传输。目录输入来自受信任的宿主部署加载器，尚未实现整个发现过程的防并发路径替换协议。

这部分已由 WASI 文件能力测试及内存 Body 流测试验证；DHTTP 接入、热更新生命周期编排、日志/handle 总额度和按时间窗口的 CPU 调度仍是后续工作。以下章节中的完整能力策略属于设计目标。

## 1. 一个身份一个沙盒

一个规范化 `server_name` 代表一个人，对应一个 Endpoint 和一个 WASM 沙盒。该身份下的所有 `.wasm` 都在这个沙盒的约束下执行，共享宿主能力上限、资源总账户和身份取消域；每个 WASM 独享自己的持久数据目录。WASM 文件是代码与 API 发布单位，也是文件能力的隔离单位，不另建 Endpoint 或身份资源总账户。

```text
DHTTP_HOME/alice/
  ssl/
  db/
    config.db                # alice 的监听设置和代理规则，仅宿主读取
    access.db                # daccess 权限库，仅宿主读取
  file/
  lib/
    profile/
      lib.wasm
      data/
    notes/
      lib.wasm
      data/
  logs/
  repo/
  templates/
```

共享沙盒不等于共享一个 Store 或一块线性内存。每次调用仍创建独立 Store、Instance、ResourceTable 和请求私有 `/tmp`，只获得当前 Lib 数据目录的受限句柄，并引用同一身份资源账户。编译后的不可变代码可以缓存；运行过的可变 guest 状态不跨请求复用。

同身份的 Lib 默认不能读写彼此的数据。宿主根据已验证的 `(server_name, LibId)` 选择 `lib/<LibId>/data/`，不接受 guest 提供宿主路径；各 Store 内 `/data` 只映射这个子目录。只预打开 data，不开放 Lib 根目录或 lib 父目录，因此 guest 无法修改 lib.wasm 或其他 Lib 数据。代码和数据放在同一 Lib 目录下，不改变目录能力的隔离边界。

首版仅扫描 `lib/` 的直接子目录，目录名就是 LibId；拒绝隐藏/临时目录、非法名称及规范化或大小写碰撞，不递归把 data 内的文件识别为 Lib。不同身份的目录、身份上下文及资源账户也严格分开。

同一 Lib 的不同 API 和请求共享该 Lib 数据，Store 按宿主批准的 Lib 文件能力配置；同一份 wasm 安装到不同 LibId 时拥有不同数据目录。LibId 不变的更新保留原目录，改名视为新 Lib，不自动迁移数据。首版不默认提供跨 Lib 共享目录；若以后需要共享，必须通过宿主显式授予独立目录能力。静态、代理、原生 handler 仍受 Router 外层身份检查和资源限制约束，原生代码不因 middleware 获得 WASM 或操作系统隔离。

## 2. 每个 WASM 内嵌一份 OpenAPI

每个 `lib/<LibId>/` 部署固定名称的 `lib.wasm`，其 component 顶层必须有且仅有一个名为 `pishoo:openapi` 的 WASM 自定义段（section id 0）。自定义段名称之后的全部 payload 是完整的 UTF-8 OpenAPI 3.1.x JSON 文档（接受常见的 3.1.0），不是文件路径、URL 或压缩包；下列 YAML 仅用于阅读示例。宿主从最终 component 字节中静态提取，不实例化或执行 guest，也不要求导出 `openapi()`、`apis()` 或私有 manifest。缺少或重复顶层同名段、无效 UTF-8/JSON 或超出大小预算均拒绝候选；嵌套 core module 中的同名段不算顶层段。WASM 与文档共同组成一个不可分割的发布文件。

标准 OpenAPI 字段描述 method/path、operationId、参数、请求体、响应和 schema。可选默认访问规则使用合法的 `x-access` 扩展；标准工具可以读取文档，但不会自动理解或执行这些扩展。OpenAPI 的 `security` 不能单独表达“访问身份等于当前 Server”；也不能把 DHTTP 身份伪装成一个客户端可填写的 header API key。

```yaml
openapi: 3.1.1
info:
  title: Profile
  version: 1.0.0
paths:
  /avatar:
    get:
      operationId: avatar.read
      x-access:
        allow: ["*?"]
      responses:
        '200':
          description: 当前身份的头像
          content:
            image/png: {}
    put:
      operationId: avatar.replace
      # 不声明默认规则：沿用 daccess 的 owner allow / 其他 deny 基线
      requestBody:
        required: true
        content:
          image/png: {}
      responses:
        '204':
          description: 头像已更新
        '403':
          description: 当前访问身份没有修改权限
```

`lib/` 下的目录名决定 LibId，所有 WASM 统一挂载到 `/api/<LibId>`：`lib/profile/` 中的上述文档挂到 `/api/profile/avatar`；`lib/index/` 同样挂到 `/api/index`。OpenAPI 中仍写应用内路径 `/avatar`。`/api` 命名空间专供 WASM，未声明入口返回 404，不回退到静态或代理。OpenAPI `servers` 不决定宿主身份、网络目标或挂载位置，避免文档替换 Router 绑定。操作唯一标识为 `(LibId, operationId)`。

首版加载 Profile 的明确限制：

- 普通 OpenAPI 不需要 Pishoo 扩展即可注册路由；method/path 是授权键，operationId 可选，用于文档和日志，出现时必须唯一。`x-access` 是可选的 Lib 默认权限建议；文件、出站与资源能力由宿主配置，不在 OpenAPI operation 中声明。未知 access effect 或非法主体拒绝，普通扩展仅作为文档。
- 首版只支持显式方法和字面量路径；模板路径、callbacks、webhooks、路径级 `$ref` 等影响路由的未支持构造拒绝加载。后续可扩展，不能默默漏注册。
- schema 允许文档内 `$ref`，有大小、深度和解析预算；禁止外部文件及网络 `$ref`。拒绝重复 JSON key。
- 拒绝重复 method/path、保留路径、编码歧义、跨平台大小写碰撞和与静态/代理分支的冲突。
- 未声明路径返回 404；已声明路径但方法不匹配返回 405。关闭自动 HEAD 到 GET 等业务回退；HEAD/OPTIONS 若需调用业务必须声明。
- OpenAPI 输入输出 schema 用于文档和 SDK；首版不承诺通用运行时 schema 校验。身份、方法、路径、请求大小与资源限制由宿主强制执行，图片格式等业务校验由 handler 处理，不因此收集完整流式 body。

### 2.1 自动生成

无需依赖 `pishoo::api` 或 Pishoo SDK。使用现有框架及其 OpenAPI 工具导出文档即可：

- Rust / Axum：`utoipa + utoipa-axum` 将路由注册与文档收集结合；operationId 默认可来自函数名，schema 可由类型生成。
- Rust / Axum：`aide` 使用 `ApiRouter`、类型推导及 builder 补充说明，更适合减少 handler 注解；当前文档说明其生成 OpenAPI 3.1.0。
- Python / FastAPI：框架自动生成 OpenAPI。这证明文档接入与语言无关，不表示 FastAPI 应用可直接编译成 WASI HTTP component。

优先复用框架的同一份路由注册生成文档。只有 method/path 或裸字节类型时，工具无法推导完整业务 schema、错误响应和“谁可调用”的语义，这些仍需开发者声明。不能从任意编译好的 WASM 完整反推 OpenAPI。

下面的 utoipa 注解逐项对应前面的 GET /avatar 与 PUT /avatar。函数体省略，仅展示文档声明；实际 handler 仍需返回对应状态、Content-Type 和流式 body，注解本身不注册 WASI 路由或执行授权。

```rust
use serde_json::json;
use utoipa::OpenApi;

#[utoipa::path(
    get,
    path = "/avatar",
    operation_id = "avatar.read",
    extensions(("x-access" = json!({ "allow": ["*?"] }))),
    responses((
        status = 200,
        description = "当前身份的头像",
        content(("image/png"))
    ))
)]
fn read_avatar() {
    // 实际 handler：读取当前 Lib 的头像，以 image/png 返回内容。
}

#[utoipa::path(
    put,
    path = "/avatar",
    operation_id = "avatar.replace",
    request_body(required = true, content(("image/png"))),
    responses(
        (status = 204, description = "头像已更新"),
        (status = 403, description = "当前访问身份没有修改权限")
    )
)]
fn replace_avatar() {
    // 实际 handler：接收 PNG、校验并保存，成功返回 204。
    // 403 由宿主权限检查产生，也属于对外 API 契约。
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Profile", version = "1.0.0"),
    paths(read_avatar, replace_avatar)
)]
struct ApiDoc;

// 在构建期文档导出程序中调用；不由 guest 写部署目录。
fn export_openapi() -> Result<(), Box<dyn std::error::Error>> {
    let document = ApiDoc::openapi().to_pretty_json()?;
    std::fs::write("openapi.json", document)?; // 构建中间产物，随后嵌入 lib.wasm
    Ok(())
}
```

| Rust 声明 | 对应 OpenAPI 字段 |
| --- | --- |
| `get` + `path = "/avatar"` | `paths./avatar.get` |
| `operation_id = "avatar.read"` | `operationId: avatar.read` |
| `extensions(("x-access" = json!({ "allow": ["*?"] })))` | `x-access.allow: ["*?"]` |
| `status = 200` | `responses.'200'` |
| `description = "当前身份的头像"` | `responses.'200'.description` |
| `content(("image/png"))` | `responses.'200'.content.image/png: {}` |
| `request_body(required = true, content(("image/png")))` | PUT 的 `requestBody.required` 与 `content.image/png` |
| `info(title = "Profile", version = "1.0.0")` | 根级 `info.title` 与 `info.version` |

`content(("image/png"))` 只声明媒体类型、不添加 schema，因此对应示例中的空对象 `{}`；它不表示响应没有内容。若写 `body = Vec<u8>`，生成器可能额外产生字节数组 schema，与这里的声明不同。`operationId` 显式设置才能得到带点的 `avatar.read`；省略时 utoipa 默认使用函数名 `read_avatar`。

上面的 YAML 是接口相关字段的展示，不要求生成文件逐字相同：生成器可以附加 tags/components，OpenAPI 版本也可能是 3.1.0，Pishoo 按已支持的 3.1.x 解析。构建时把生成的 JSON 嵌入最终 component 的 `pishoo:openapi` 段，只发布 `lib/profile/lib.wasm`；中间 `openapi.json` 不部署。示例语法依据 [utoipa path 的 content 与 extensions](https://docs.rs/utoipa/latest/utoipa/attr.path.html)，本段未编译运行。

可选默认权限可以用生成器现有的 OpenAPI extension 接口加入，或在构建后补入 JSON；不必开发 Pishoo 宏。Lib 不声明默认权限也可部署，使用 daccess 的基础策略。

```text
普通框架路由 / handler / 类型
  ├─ 原有业务路由与代码 → WASI HTTP component
  └─ 现有 OpenAPI 生成器 → 标准 OpenAPI JSON（可选补充默认权限建议）
两份构建产物 → 在最终 component 顶层嵌入 pishoo:openapi → lib.wasm
```

OpenAPI 生成与 WASI 编译是两个问题。运行产物仍须实现标准 WASI HTTP p2；现有 Axum 或其他服务器的 socket 启动代码不会因导出了 OpenAPI 而自动变成 WASI handler。具体运行适配需单独验证，但不应让文档导出依赖 Pishoo SDK。

工具依据：[utoipa path](https://docs.rs/utoipa/latest/utoipa/attr.path.html)、[utoipa-axum](https://docs.rs/utoipa-axum/latest/utoipa_axum/)、[aide](https://docs.rs/aide/latest/aide/)、[FastAPI OpenAPI](https://fastapi.tiangolo.com/advanced/path-operation-advanced-configuration/)。

#### Node.js：用 swagger-jsdoc 导出

Node.js 可以使用 [swagger-jsdoc](https://github.com/Surnet/swagger-jsdoc) 从函数旁的 `@openapi` 注释生成标准文档，无需 Pishoo SDK。以下仅演示最小接口声明与构建期导出，业务函数为占位；注释不会自动注册路由，也不会编译 WASM。

```sh
npm install --save-dev swagger-jsdoc@6
node export-openapi.cjs
```

`export-openapi.cjs`：

```javascript
const swaggerJsdoc = require('swagger-jsdoc');
const { writeFileSync } = require('node:fs');

/**
 * @openapi
 * /avatar:
 *   get:
 *     operationId: avatar.read
 *     x-access:
 *       allow: ["*?"]
 *     responses:
 *       '200':
 *         description: OK
 */
function readAvatar() {
  // 业务 handler 占位；实际实现读取并返回头像。
}

/**
 * @openapi
 * /avatar:
 *   put:
 *     operationId: avatar.replace
 *     responses:
 *       '204':
 *         description: Updated
 */
function replaceAvatar() {
  // 业务 handler 占位；实际实现保存头像。
}

const document = swaggerJsdoc({
  failOnErrors: true,
  definition: {
    openapi: '3.1.0',
    info: { title: 'Profile', version: '1.0.0' },
  },
  apis: [__filename],
});

writeFileSync('openapi.json', JSON.stringify(document, null, 2) + '\n');
```

注释的 `/avatar`、get/put、operationId、responses 和可选 x-access 直接对应导出文件中的同名字段；不需要 requestBody 或响应 schema。两个同路径 operation 合并到 `paths./avatar`。`x-access` 可省略，实际授权仍由 daccess 执行。导出文件中使用应用内路径 `/avatar`，部署到 `lib/profile/` 后由 Pishoo 挂为 `/api/profile/avatar`。

该工具读取注释而不推导或校验业务分发，开发者需保持实际 router 与文档一致。运行此脚本只生成构建中间文件 `openapi.json`，还须将其嵌入最终 component；JavaScript 业务代码生成标准 WASI HTTP component 属于另一个构建步骤，不能直接把 Node.js 程序或依赖视为 lib.wasm。本段根据官方 API 编写，未安装依赖或执行。

### 2.2 单文件加载与更新

构建工具应在编译生成 component 之后，把最终 OpenAPI JSON 写入其顶层 `pishoo:openapi` 自定义段，并对最终 `lib.wasm` 执行与运行时相同的提取、解析和组件校验。具体嵌入工具尚待选定；不能把本段示例中的导出脚本视为已完成嵌入。文档如需单独供 SDK 或人工查看，可从最终 component 提取，但提取副本不是部署输入或路由真相源。改变文档也必须生成新的 `lib.wasm`。

宿主从一个有界、稳定的 `lib.wasm` 文件快照静态提取 OpenAPI，校验文档、可选扩展、路由及组件 imports，再编译同一份字节并记录整个文件的摘要。文档中不要求也不校验指向自身的组件摘要。解析或校验失败不发布该候选；热更新保留该 Lib 旧版本，首次加载跳过该 Lib。发布者先在同目录写临时文件、完成校验并以原子重命名替换 `lib.wasm`；宿主若观察到写入中的不稳定内容则重试或保留旧版本。Router、权限建议和组件实例必须绑定同一文件快照；文件摘要不是签名或授权凭据，也不能证明 handler 行为符合文档。

Lib 目录或 `lib.wasm` 确认删除且父目录可稳定读取时移除 Lib；暂时读取失败或内容无效属于坏更新，不当作已授权删除。运行数据更新不触发路由重建；监听只关注 Lib 目录发现/移除及 `lib.wasm` 变化。更新组件时保留 data，不整目录覆盖或清空。移除部署文件可以保留 data；宿主不会自动清理数据。guest 无法写 lib.wasm。身份所有者授权与平台上限由宿主控制，上传一份自称公开的文档不是普通访客可执行的操作。

## 3. 逐 API 访问策略

所有请求先经过 Pishoo 路由匹配，再由宿主调用该身份的 daccess 完成授权。WASM 不需要重复判断“是否本人”。表中扩展是 Lib 提供的可选默认权限建议，不是绕过管理员规则的第二个授权入口。

OpenAPI operation 的自定义字段必须使用 `x-` 前缀，所以文档字段简化为 `x-access`，代码/界面中称 access。不再使用 server-self/authenticated/file 这套枚举，直接沿用 daccess 的 effect 与 grantee：

```yaml
x-access:
  allow: ["**"]
  review: ["?"]
```

method/path 已由 operation 决定，不重复声明。`allow`、`review`、`deny` 的值是主体列表：`**` 表示所有具名主体，`?` 表示匿名，`*?` 表示所有主体，具体名称表示已登记联系人。同一主体不得同时出现在多个 effect；`*?` 与 `**`/`?` 的冲突按 daccess 约束拒绝，不靠 JSON 字段顺序解决。默认导入应保留 owner 基线且不覆盖管理员规则；具体联系人必须符合 daccess 的已登记要求。省略或空对象表示不提供初始规则，不表示公开。`allow` 不代表跳过 SubjectId 及联系人状态检查，`deny` 也不拥有高于所有更具体规则的全局优先级，最终按 daccess 的匹配规则决定。

utoipa 原生写法，无 Pishoo 宏依赖：

```rust
extensions(("x-access" = json!({ "allow": ["*?"] })))
```

以上例子适合公开 GET；只允许本人修改的 PUT 可省略扩展，使用基础策略。默认建议仍由受信任部署方接受，不直接成为新的运行时授权来源。

每个身份独立创建 AccessService，使用宿主持有的 `db/access.db`，不向 guest 开放。现有 daccess 基础策略为 deny all、allow profile owner；调用者必须提供可信名称和 SubjectId，遵循其 allow/review/deny 与主体变更检查。新 Lib 的默认建议经部署方接受后作为初始规则导入，不覆盖既有管理员策略，更新不自动重新导入或复活已删除规则。默认导入的来源跟踪、冲突检查及路由发布协调仍是待实现接缝；实现前不应用默认建议，沿用既有 daccess 决策，不能在 Denied 后另用 Lib 默认 allow 放行。OpenAPI `security` 只描述认证要求，不据此推断同名角色或 OAuth scope 等于 daccess 授权；未知映射不能自动放行。

握手失败、缺失必需的可信元数据不能当作匿名；本端身份与 authority 必须匹配 Router。静态、代理继续仅自身访问，管理路径保留专门授权，OpenAPI 无法放宽这些分支。

在默认自身访问规则下，Alice 的 `PUT /api/profile/avatar` 只有 Alice 可调用，Bob 和匿名在实例化 guest 前收到 403；身份管理员可通过 daccess 显式调整。目标是 alice 沙盒内的数据，请求载荷不能切换到 Bob 的目录。公开 GET 只开放读取，不使 PUT 自动开放。

OpenAPI 不声明每 API 的执行模式。文件读写、出站和资源限制由 Pishoo 根据身份及 Lib 的宿主策略配置到 Store；每个 Lib 只能访问自己的数据目录，获准的 file 目录仍只读。daccess 决定谁能调用 API，宿主能力决定代码能做什么。

允许公开 GET 不会自动把该次 Store 变成只读；如果 Lib 获得自身 data 写权限，其 GET handler 也具有该能力。接口的读取/修改业务语义由 Lib 实现，不能宣称宿主按 HTTP 方法强制阻止副作用。需要整个 Lib 只读时，由宿主限制该 Lib 的目录能力。

### 3.1 首版权限管理边界

首版由前端以真实用户身份直接调用 daccess 的联系人、授权、撤销和审批 API；daccess 校验该用户是否有权执行管理操作。前端不持有绕过鉴权的管理凭据，公开的好友申请也不等于接受好友或授予权限。WASM 只执行业务请求，不提供修改 ACL 的宿主能力或本地管理入口。

```text
前端 ──权限管理请求──→ daccess（检查管理权限并变更规则）
前端 ──业务请求────→ Pishoo 路由 → daccess 检查 → WASM
```

以 Alice 接受 Bob 并允许其读取私有头像为例：前端遵循 daccess 联系人状态机登记、确认 Bob，并以有管理权限的 Alice 身份调用现有规则接口（最终挂载前缀另定）：

```http
POST /acl/access/api/profile/avatar
Content-Type: application/json

{"method":"GET","effect":"allow","grantee":"bob.example"}
```

此规则只授予 GET，不授予 PUT。Bob 必须先符合 daccess 的联系人登记要求；需要在批准回调中发送授权清单时，遵循现有“写规则 → local_approval → 通知”的顺序。多步骤未全部成功时，前端应显示实际状态并支持重试，不能把 Lib 内的 friend=true 当成授权已完成。

静态 `x-access` 仅提供部署时的默认建议，运行时好友和分享变更只写 daccess，不回写 OpenAPI。管理员规则不因 Lib 更新重置。前端可直接完成分享、撤销分享和公开范围调整；这些操作无需 WASM 中转。

这一限制必须落实到所有宿主入口：不暴露 access.db，不导出 ACL 写 host API，也不允许 guest 通过 outgoing、自调用或管理代理借用 Server 身份修改规则、决定审批或调用会间接改变授权的联系人管理操作。宿主对实际管理目标执行拒绝，不能被普通 outgoing allowlist 覆盖；路径规范化、重定向和管理代理不得绕过检查。即使本次入站请求来自 owner，guest 也不因此获得首版未开放的授权管理能力。

### 3.2 业务自动授权留作后续扩展

付款成功后开放下载、加入组织后开放接口等场景，需要可信业务结果触发权限变更，不能让前端自行声称“已付款”或“已加入”。首版不提供通用 WASM 自动授权机制；出现具体需求后，再单独设计受限宿主接口、可信事件验证及重试/撤销流程，不作为首版交付前置。

订单是否已支付、文章是否已发布等业务状态仍由业务实现维护，并非每个状态都要转换为 ACL。若该状态需要改变谁能访问，应由后续可信流程交给 daccess，不能由 WASM 绕过 Pishoo 的准入判断。按文章所有者等资源级授权还需要可信资源归属与存储约束，现有 method/path ACL 不宣称覆盖此类需求。

现有 daccess 规则按路径前缀匹配：`/api/profile/avatar` 也可能覆盖其子路径。删除 Bob 的精确 allow 后，其他具名/所有人或父路径规则仍可能允许访问；撤销应检查最终有效规则。完整删除联系人会删除其全部精确规则，影响其他 Lib，不能当成只撤销头像权限。

现有接口依据：[daccess README](../../daccess/README.md)、[daccess SPEC](../../daccess/SPEC.md)。以上是 Pishoo 首版设计限制，不表示相关宿主拦截已实现。

## 4. 请求分发与流式执行

HTTP Body、WASI 资源及执行任务的结构与转换详见 [WASM HTTP 适配层](wasm-http-adapter.md)。

```text
可信 dhttp 握手事实
  → 校验本端身份 / authority，Server 总准入
  → 按 OpenAPI 匹配 method + path
  → daccess 检查可信访问主体、method 与规范化实际路径（包括挂载前缀）
  → 从身份沙盒账户预留资源，应用宿主配置的 Lib 能力及调用者限制
  → 新 Store / Instance，调用目标 wasm 的 WASI HTTP handle
  → 持续驱动请求 / 响应 body，完成或取消后释放资源
```

宿主清洗所有入站 `pishoo-*`，注入 server-name、lib-id、可选 api-id（operationId）与可选 client-identity。宿主鉴权读取可信 extensions，不读取这些 ABI 投影。出站和响应过滤保留头。

guest 按标准请求 method/path 使用自己的 router 分发，默认不依赖 `pishoo-api-id`；宿主只剥离一次 `/api/<LibId>` 挂载前缀并保留 query。operationId 头仅在存在时供可选诊断或自愿集成使用，不是强制 ABI。宿主与 guest 使用一致的路径规范化语义，实际能力仍由宿主限制，宿主不会静态证明 guest 行为。

WASI HTTP p2 通过 incoming-request / response-outparam 及 body 流资源交互，授权后不收集完整 body。提交响应头后 guest 可以继续读写；Store、身份预算与取消关系必须保持到 guest、host I/O 和实际输出均结束。

HTTP 自调用、组件间 HTTP 调用一律重新经过 Router。只允许可信访问身份等于 Server 的调用进一步使用该 Endpoint 出站，且满足身份 outgoing allowlist 和本次能力限制；公开访客不会获得借用 Server 身份的权限。远端看到当前 Server 传输身份，不自动继承原始访客。原生 socket、任意证书和私钥始终不向 guest 开放。

## 5. 文件、资源和生命周期

只按稳定目录句柄开放当前身份下当前 Lib 的 `/data`、获准的只读 `/file` 及请求私有 `/tmp`；不挂整个 Server 根目录。ssl、私钥、wasm、OpenAPI、宿主数据库、其他 Lib 数据目录和其他身份目录不可见；拒绝链接、特殊文件及路径逃逸。

资源按全局、身份、单次调用核算；同一身份所有组件和旧新代码版本共用总账户。涵盖并发、排队、线性内存、CPU fuel/epoch、host I/O deadline、句柄、日志、磁盘和传输/出站。单块 memory 上限不等于总内存限制；目录授权不提供磁盘配额；普通 timeout 不保证 guest 已被中断。

按组件记录使用量、调度公平性和取消标签不改变身份总额度归属；文件能力则按 Lib 隔离。移除一对 Lib 文件停止该组件新调用并取消其在途任务，不销毁身份沙盒，也不自动删除该 Lib 的持久数据；其他组件继续。删除身份则停止所有组件、取消所有请求并关闭 Endpoint。

每次请求固定 Router、组件及能力快照。授权收紧影响新请求，在途仍使用旧快照；立即撤权需显式取消旧执行。更新不能重置聚合账户，同一 Lib 的旧新版本须处理其数据的并发及版本兼容，其他 Lib 不可访问该目录。

### 5.1 身份签名与验签宿主接口

WASM component 通过 [`pishoo:identity/signatures@0.1.0`](../wit/pishoo-identity/identity.wit) import 调用两个函数。这是 guest → 宿主的组件接口，不是 OpenAPI 中发布的 HTTP 路由；使用者需在编译 component 时导入对应 WIT 包，dhttp 的组件 Linker 在实例化前连接宿主实现。不导入此接口的 Lib 不受影响。接口版本变更按 WIT 包版本处理，不凭 HTTP header 或 OpenAPI 扩展切换。

```wit
sign: func(data: list<u8>) -> result<list<u8>, sign-error>;
verify: func(signature: list<u8>, data: list<u8>, identity: string) -> result<bool, verify-error>;
```

`sign` 的输入与输出都是原始字节；宿主只从当前请求已绑定的 Server/Endpoint 取得 `LocalAuthority`，调用其 `sign(data)`。guest 无法传入签名身份、私钥路径、算法或证书。此能力应由受信任的部署配置按 LibId 授予，不能因为组件声明了 import 就自动开放；未获授权返回 `denied`，不尝试签名。每次请求的 `StoreData` 仅持有指向当前 Server 签名服务的受控句柄，不复制私钥给 guest。每次签名计入该身份与请求的操作额度，限制输入长度、并发数和执行时长，取消后停止后续签名；异步或硬件密钥实现不得阻塞 Wasmtime 执行线程。

`verify` 先规范化 guest 传入的 DHTTP 身份名称，按宿主的可信身份解析规则查找该名称绑定的证书/公钥，再调用 DHTTP 身份验签逻辑。不能把 guest 提供的证书当作可信证书，也不能只比较证书上的自称名称。证书查找可以缓存，但须遵守可信来源、有效期、撤销和更新策略；同名证书轮换时，应按业务所需的签名有效期查找仍被信任的历史证书。仅给身份名称、内容和裸签名时，可能需要逐一尝试候选公钥；若后续需明确是哪一把密钥，应另行设计带密钥标识的签名封装格式。`true` 表示有可信候选证书验签成功，`false` 表示已找到可信证书但签名与内容不符；无此身份、无可信证书、解析故障等返回相应错误，不能与 `false` 混淆。

签名与验签都作用于调用方传入的**精确字节**，不做 JSON 规范化、文本编码转换或自动拼接。业务方必须先定义消息格式，包括用途、版本、身份和必要的时间/随机数，再把编码后的字节交给 `sign`；验证方验证签名后仍须检查这些业务字段，防止签名被跨用途使用或旧消息被重放。接口不代表签名者授权某个访问方，也不替代入站 daccess 检查。上游 `dhttp-identity` 已提供 `LocalAuthority::sign` 和 `RemoteAuthority::verify`；按名称解析远端可信证书及通用组件宿主 import 注入尚未实现，见 [DHTTP 接入说明](dhttp-integration-contract.md#4-接入时补齐的细节)。

### 5.2 用一个头像 Lib 看权限如何生效

以下是**说明流程的伪代码，不是 Wasmtime 的真实 API**。先记住两件事：OpenAPI 写的是“访客可以请求哪些网址”；Linker 连接的是“WASM 可以向宿主请求哪些基础服务”。例如，`GET /api/profile/avatar` 属于前者，“读文件”属于后者。

**进程启动：备好运行工具，不启动任何 Lib。**

```text
engine = 创建 WASM 执行器()
engine.开启计算额度(fuel)       # 用完就停止，防止一直计算
engine.启用 epoch 中断检查()   # epoch 是宿主推进的计数器，不是时间单位
宿主定时推进 engine.epoch      # 让一直计算的 WASM 也有机会被中断

linker = 创建宿主服务连接表()
linker.连接("WASM 请求打开/读取/写入文件", 受控文件服务)
linker.连接("WASM 请求读取上传内容/写回响应", 受控 HTTP 服务)
linker.连接("WASM 构造普通 WASI HTTP 请求", WASI HTTP outgoing-handler)
# 这里连接的是宿主基础服务，不是 GET /avatar 这样的业务 API。
# Linker 只接入接口；每次请求的 Store 装入 DhttpHooks，决定能否发出。
# 能连接到服务，不等于每个 Lib 都获准使用：实际目录和出站目标在 Store 中限制。
# 非自身身份调用、未批准目标、ACL 管理目标一律拒绝出站。
# 不连接原生 socket、宿主环境变量、ACL 管理服务。

编译队列 = 有界队列(最大等待数, 最大同时编译数)
全局额度 = 创建资源账户()
```

**发现 Alice：检查配置并发布 Lib，但仍不运行 guest。**

```text
校验("alice" 目录名、身份凭据、证书)
读取 Alice 的宿主权限配置                  # 只能由受信任部署方设置

profile = 读取 lib.wasm 稳定快照并提取顶层 pishoo:openapi 段
检查 OpenAPI 里声明了 GET /avatar          # 这是访客请求入口
检查 lib.wasm 只需要受支持的 WASI 接口      # 这是 WASM 要用的宿主服务
编译 profile 的 WASM；保存编译结果          # 不创建正在运行的实例

profile权限 = {
    /data: 读写 Alice/profile 自己的目录,
    /file: 只读,
    出站 HTTP: 关闭,
}
notes权限 = {
    /data: 只读 Alice/notes 自己的目录,
    /file: 不提供,
    出站 HTTP: 关闭,
}
# 两个 Lib 都可能调用“文件接口”，但拿到的目录和读写权不同。
# 如果 Alice 的 Server 总策略禁止写，profile 的 /data 也变为只读。

创建 Alice 的并发、内存、文件句柄、日志等额度
全部检查和编译成功后，发布 SandboxRuntime + 路由
# 中途失败：新版本不发布；热更新继续使用旧版本。
```

**Bob 请求 Alice 的头像：先决定能不能调用，再决定 WASM 能做什么。**

```text
请求 = Bob 发来 GET /api/profile/avatar
校验真实来访身份是 Bob，目标 Server 是 Alice
找到 OpenAPI 中的 GET /avatar

daccess.检查(Bob, GET, /api/profile/avatar)
# 不允许：直接返回 403；不创建 WASM 实例。
# 允许：继续。OpenAPI 的 x-access 不能跳过这一步。

取得全局、Alice、profile 的并发与资源额度
本次权限 = Alice 的总上限 ∩ profile权限 ∩ 本次调用限制
store = 新建 Store(本次权限、资源额度、取消令牌)
store.挂载(/data = Alice/profile/data, 按本次权限读写)
store.挂载(/file = Alice/file, 只读)      # 未获准则不挂载
store.挂载(/tmp = 本次请求的临时目录)
store.不继承(宿主环境、stdin、home、密钥、数据库)
最长执行时长 = 宿主策略.max_duration       # 例如 2 秒；这是对外配置项
停止时刻 = 当前时间 + 最长执行时长
store.安装(内存限制、fuel)
store.设置 epoch 中断点(根据最长执行时长换算为 tick 数)
Supervisor.设置超时(停止时刻)             # 到时取消整个请求和 host I/O
store.HTTP出站处理器 = DhttpHooks(当前 Alice Endpoint, 本次权限, Bob, 取消令牌)
# Bob 来自可信入站握手；guest 不能自填调用者或源身份。
# 本例 Bob != Alice，按首版策略拒绝出站；若允许，DHTTP 源身份仍是 Alice。

instance = 用编译结果新建 WASM 实例(store)
执行 GET /avatar；边读边传输响应
Supervisor 等待 guest、文件 I/O 和实际输出结束
销毁 instance 和 store；归还资源额度
# 超时、断连或撤权时：先中断并等待清理，再归还额度。
```

配置的是 `max_duration`；“停止时刻”只是本次请求开始后算出的内部值。epoch 按 tick 检查，不能单独充当精确计时器；Supervisor 的时钟超时负责整体时长，epoch 负责让纯计算中的 WASM 能响应取消。

运行中仍要计量：单块 memory、单次请求和 Alice 总内存；文件/body/pollable 句柄；磁盘写入、日志、HTTP 缓冲和出站流量。`preopened_dir` 只决定“能看到哪个目录”，不自动限制目录容量，也不足以单独处理路径替换或即时撤权；生产版需要受控 filesystem host 层或 OS project quota。出站默认关闭；获准后每次发送和重定向仍检查目标，非 Alice 本人调用不能借用 Alice 的出站身份，自调用重新经过路由和 daccess，ACL 管理始终禁止。

## 6. 实现验收清单

- 同身份多个组件各自独享 data，共享身份资源总额度；并发调用不因换组件或更新逃逸限额。
- 同身份 Lib A 无法读写 Lib B 的目录；父目录访问、名称碰撞、链接和自定义文件 host API 不能绕过目录限制。
- 不同身份无法访问对方目录、身份句柄或资源账户。
- 未批准的 WASI import、错误版本或缺少 incoming-handler export 拒绝加载；空策略不会生成带宿主 env、socket 或 ACL 能力的 Store。
- 普通生成器输出、无 Pishoo 扩展及无 operationId 的 OpenAPI 经内嵌后可接入；文档路由与 guest 路由一致；未声明 API 不进入 guest。
- 单个 Lib 的文件暂时不可读、缺少或重复顶层 `pishoo:openapi` 段、文档或组件无效、未知策略时，更新保留该 Lib 已发布版本，初次加载跳过该 Lib 并报告；确认删除文件或 Lib 目录时移除该 Lib；完整 Router 存在路由冲突时保留旧 Router。
- Alice 可修改本人头像，Bob/匿名被宿主拒绝；GET 不自动获得 PUT 的调用权限；配置为只读的 Lib 无法持久写入，出站遵循宿主限制。
- 伪造身份/API 头、歧义路径、自动 HEAD 和其他方法不能绕过准入。
- 前端权限管理请求按真实主体鉴权；guest 即使由 owner 调用，也无法经文件、host API、outgoing、自调用或管理代理修改 ACL、决定审批或间接授予权限。
- 流式上传、提前响应、trailers、超时、断连均完成 guest 和 host 资源回收。
- 单块 memory、请求聚合内存、Server 总内存和 host buffer 分别受限；编译队列、handle、磁盘、日志、HTTP buffer 与出站超额时均可受控拒绝，并在取消后归还额度。
- 移除单组件不关闭其他组件或删除该 Lib 的持久 data；删除身份取消全部执行。

本次只更新设计，以上验收尚未实现或运行。OpenAPI 标准及扩展依据：[OpenAPI 3.1.1](https://spec.openapis.org/oas/v3.1.1.html#specification-extensions)。

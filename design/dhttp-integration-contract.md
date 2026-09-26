# Pishoo 与 DHTTP 接入说明

日期：2026-09-22。状态：**设计对齐，未实现。** 以 [dhttp 顶层接口稿](../../dhttp/docs/api/top-level-review.md) 为上游依据；它优先于早期架构提案。本轮只读设计文档，不依据 dhttp 当前代码增加接口。

Pishoo 的主体接口见 [接口约定](pishoo-api.md)：读取每个身份的 db/config.db，为每个身份组装 Endpoint 和 Router，共用一次初始化的 Network。

## 1. 直接复用的接口

| 能力 | 使用方式 | Pishoo 的工作 |
| --- | --- | --- |
| 全局网络 | `DhttpNetwork::init(config)` / `shutdown(deadline)` | 从各 Server 的监听设置汇总底层网络范围 |
| 身份端点 | `Endpoint::load(name)` / `name()` | 从固定目录发现名称 |
| 应用监听 | `endpoint.listen(router)` | 用 Axum/Tower 组装代理、静态和 WASM |
| 停止/关闭 | `stop_listening()` / `close(deadline)` | 跟踪监听任务，按身份更新与清理 |
| 握手事实 | `HandshakeSummary` / `ArcConnection` extensions | 做固定授权，核对本端身份与 authority |
| WASM | `WasmApp::load/from_bytes`，标准 Service 实现 | 校验组件内嵌 OpenAPI，将所有同身份 `.wasm` 绑定到共享沙盒策略与资源账户 |
| 出站请求 | `endpoint.get/post/request(...)` 返回请求 future | 绑定当前身份，执行出站授权 |

Pishoo 不再设计额外的 dhttp Runtime、Registration、RequestContext 或 ExchangeScope。也不通过连接句柄自己驱动 QUIC 流、QPACK、连接池或 STUN。

Pishoo 每个 Server 的 db/config.db 保存独立监听范围，Network 底层监听规则按需求并集生成；使用上游范围规则持续覆盖适用网卡，不再自行枚举固定网卡清单。Internal/External 独立，默认每 Server 仅 Internal；端口及 STUN 使用宿主/上游默认值。

当前上游将范围限制放在全局 Network，Endpoint.listen(lib) 没有逐身份范围参数。Pishoo 需要补充目标 Endpoint 的可信入站范围准入和服务发布约束，防止一个 External Server 使其他 Internal Server 暴露。此接口待对齐，不能宣称现有接口已经满足，也不能以范围并集替代身份限制。首版修改监听范围重启生效，应用内容更新不重启 Network。

## 2. 标准服务与身份

```text
dhttp HTTP/3 接入
  → http::Request<dhttp::Body> + 可信握手 extensions
  → Pishoo 的身份检查、资源与生命周期中间件 + Router
      ├─ 静态 / 代理 / 内置原生 handler
      └─ 身份沙盒（共享能力上限、总额度；data 按 Lib 隔离）
          ├─ dhttp::WasmApp A
          └─ dhttp::WasmApp B
  → 标准 http::Response<B>
  → dhttp HTTP/3 输出
```

dhttp 接受标准 Tower Service，响应 body 保持泛型；Pishoo 使用 Axum 不要求 dhttp 依赖 Axum。正确驱动同一个 Service 的 readiness 和 call，保留 body/trailers、提前响应和背压，不先缓冲整个请求。

Router 在构建时绑定 Endpoint。本端身份和 authority 必须与此绑定一致，不能凭请求 header 切换到其他 Router。握手失败不能当匿名；必需的握手元数据缺失是接入错误。路由外层验证身份事实和 Server 绑定，WASM 分支匹配 method/path 后调用每身份 daccess，传入可信名称与 SubjectId 执行授权；静态、代理保留 self-identity 规则，管理路径执行专门授权。公开 API 仅允许 dhttp 明确标记的匿名连接，不能把缺失握手元数据视为匿名。

普通客户端 header 不产生可信身份。调用 WASM 前清除所有大小写形式的 `pishoo-*`，再注入 server-name 和可选 client-identity；向上游发送请求和返回响应前过滤该保留前缀。宿主授权始终读取握手事实。

## 3. WASM Lib 与完整请求生命周期

结构、流转换及任务回收的实现设计见 [WASM HTTP 适配层](wasm-http-adapter.md)，依据 h3x 测试区分传输适配和 WASI 执行适配。

WASI HTTP p2 的加载、编译、Store/Instance、消息转换和 guest 执行由 dhttp 提供。Pishoo 的 Router 外层中间件负责 Endpoint 身份、总预算与全部路由生命周期；每身份的 daccess 负责逐 API 授权。每个 Server 的所有 WasmApp 共享一个身份沙盒，共享能力上限和资源总账户，各 Lib 独享 `lib/<LibId>/data/`，同目录的 lib.wasm 仅由宿主加载。每个请求依然独立创建 Store/Instance，按 Lib 配置权限，Lib 目录能力由宿主按 LibId 隔离。

Pishoo 从同一份 `lib/<LibId>/lib.wasm` 快照的顶层 `pishoo:openapi` 自定义段静态读取路由及可选 operationId/默认权限扩展，记录整个组件摘要；不执行 guest 读取 metadata，也不要求上游实现 Pishoo 专属 OpenAPI 解析。dhttp 只需接受通用宿主策略与资源账户注入；原先 `apis()` 或 component manifest 不再作为路由真相源。

执行约束必须连接到 dhttp 实际使用的 Store 和任务：返回响应头之后继续保留环境与预算；完成或取消后等待 guest、host I/O 和输出清理。请求级错误只结束当前交换，不直接关闭共享 QUIC 连接。HEAD/204/304 和 body 被替换时，回收原执行但不能无条件 reset 最终响应。

## 4. 接入时补齐的细节

上游稿已经决定主干接口，下面几项还未写出具体接缝。它们留在装配或适配函数内部，不成为新增 Pishoo 框架的理由。

| 细节 | 已定要求 | 上游尚未定义的部分 |
| --- | --- | --- |
| 逐 Server 监听范围 | 从各自 db/config.db 读取设置，共享 Network 只汇总底层资源 | 按目标 Endpoint 的范围准入及服务发布约束；现有全局 scopes 不足以实现 |
| 凭据目录 | 使用 `DHTTP_HOME/<身份名>/ssl/fullchain.crt`、`privkey.pem` 和 `ocsp.der` | 通过 `dhttp-home` 定位身份目录；Endpoint 的公开入口仍是 load(name) |
| 内嵌 OpenAPI | Pishoo 从最终组件快照静态提取并校验唯一的顶层 `pishoo:openapi` 段；单文件原子替换 | 不要求 dhttp 新增 manifest WIT；通用 component imports 检查接缝待对齐 |
| API 准入 | Pishoo 按 OpenAPI 匹配并交给每身份 daccess 授权，未声明入口不进入 guest；绑定同一内容和策略快照 | 通用可信调用元数据的注入接缝；具体访问策略由 Pishoo 解释 |
| WASM 宿主设置 | Lib 私有目录、身份 outgoing、总限额、取消及 Lib 能力配置作用于真实 Store | 多个 WasmApp 接收共享身份预算及请求级能力的宿主注入签名 |
| 身份签名与验签 | 以 `pishoo:identity/signatures@0.1.0` 组件 import 提供；签名固定使用当前 Server 的 `LocalAuthority`，验签使用目标身份的可信证书；签名能力按 Lib 授权 | dhttp 通用组件 Linker 的自定义接口注入、按规范化名称解析受信任 `RemoteAuthority` 及历史证书的接缝尚待对齐 |
| daccess 身份及默认规则 | 名称与 SubjectId 从可信连接适配，每身份独立 AccessService；Lib 默认建议不绕过现有规则 | 可信 SubjectId 的映射、默认导入来源跟踪及发布协调待实现 |
| 首版权限管理限制 | 前端以真实用户身份直接调用 daccess；guest 不得写 ACL、决定审批或通过联系人操作间接授权，outgoing/自调用同样受限 | 实际目标与重定向检查须落实到宿主；WASM 自动授权接口不在首版范围 |
| 完整出站请求 | 使用当前 Endpoint 传递 headers、流式 body 和 trailers | 便捷方法目前只给出 method/url 输入；标准 body 适配细节待补 |
| 请求完成 | 覆盖正常输出、取消、service 失败和 body 替换 | 实际输出及执行任务终态的宿主通知方式 |

不能把默认 WasmApp 外包一层 timeout 就当作身份聚合限额和请求能力已经生效，也不能用 Body EOF 代替网络输出完成。这些接缝由 dhttp 的通用机制提供，不引入它对 Pishoo 的反向依赖。

身份沙盒覆盖一个 Endpoint 下的全部 WASM，与当前一个身份一个执行边界的方向一致；共享沙盒不表示所有请求复用一个 Store。dhttp 负责通用 WASI HTTP 执行，Pishoo 负责具体授权、共享账户和路由装配。

身份热轮换和远程终端的 Extended CONNECT 双向流接缝不在 dhttp 本轮顶层接口范围，暂不冻结新上游方法。首版终端不依赖 WebTransport，不以其适配或测试完成作为前置。身份首次实现不承诺无中断续期：加载凭据后保持该身份，替换有效凭据需重启加载；新材料错误保留仍有效的已加载材料，当前材料失效则停止该身份。

`/shell/<run_user>` 继续保留。首版采用一个 HTTP/3 Extended CONNECT 请求对应一个 shell/PTY，多个终端由 H3 请求流复用，不增加 DShell 会话内多 channel 层。双向流接缝尚未接入时，先检查身份和账号，合法请求返回 501，不启动 shell；接入后按 [整体设计](pishoo-wasm-db-redesign.md) 的同账号终端与消息协议约定执行。客户端需同步适配，不承诺与现有 DShell over WebTransport 或标准 SSH 客户端直接兼容；单个终端关闭不得关闭共享连接或其他终端。

## 5. 验收重点

验证两个身份共享网络且互不误关；static/proxy/WASM 都经过 Router 外层身份检查与资源限制；同一 Server 的多个 WasmApp 独享各自 data、共享身份总额度、不同 Server 隔离；内嵌 OpenAPI 缺失、无效或越权只跳过该 Lib 的错误候选，首次加载不阻止该身份启动，更新保留该 Lib 已发布版本；代理变化只替换 Router；匿名、错误 authority 和伪造身份 header 被正确处理；流式上传、提前响应、trailers、取消、body 替换均完成任务回收；删除一个 Lib 不影响同 Server 其他 Lib；目录删除优先于长编译，旧任务不能影响同名新 Endpoint。

终端接缝接入后的验收还需覆盖：未启用 WebTransport 时可建立终端；同一 H3 连接上的两个终端分别授权且独立退出；窗口调整、输入结束和退出码正确传递；慢消费者不会导致无界缓冲；单流取消回收对应进程，连接中断回收该连接上的全部终端。

本轮只做文档一致性检查，未实现或运行这些集成测试。

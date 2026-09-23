# WASM HTTP 适配层：结构、流与执行生命周期

日期：2026-09-22。状态：实现设计，未实现。依据当前 [h3x WASI HTTP 测试](../../h3x/tests/wasmtime_wasi_http.rs) 与 guest fixtures；宿主测试依赖 Wasmtime 47（Cargo.lock 为 47.0.4），guest 使用 wasip2 1.0.4。下文真实 API 名来自测试，其余内部结构及流程是拟议伪代码，不是可直接编译或已冻结的 dhttp 接口。

本设计遵循 [身份沙盒与 OpenAPI](wasm-sandbox-design.md)：一个身份一个沙盒、多个 WASM 共享身份账户、每个 WASM 独享 data、每请求新 Store。普通 OpenAPI 静态注册路由，不要求 guest 导出私有 metadata；权限管理由前端直接对接 daccess，guest 不具备 ACL 修改能力。

## 1. 适配层分成两层

```text
HTTP/3 请求流
  ⇅ h3x Request<R> / Response<W>
[A] HTTP 传输适配（dhttp 服务接入层）
  ⇅ http::Request<Body> / http::Response<B>
Pishoo Router + daccess + 身份沙盒准入
  ⇅ 同样的标准 HTTP 类型
[B] WASI HTTP 适配（dhttp::wasm）
  ⇅ incoming-request / response-outparam / body resources
WASM 的 wasi:http/incoming-handler.handle
```

| 层 | 职责 |
| --- | --- |
| h3x | HTTP/3 帧、头压缩、请求流及读写缓冲 |
| A：传输适配 | h3x 流与标准 Body 转换、trailers、实际发送、流级取消与完成通知 |
| B：WASI 适配 | 加载 component、创建 Store、注册 WASI 资源、调用 guest、桥接 body、管理执行任务 |
| Pishoo | 身份绑定、OpenAPI 路由、daccess 准入、沙盒共享预算与能力配置 |

h3x 测试中的 `serve` 是 A 的原型，`handle_wasm` 是 B 的原型，`fixture_router` 是业务装配。测试用 Axum 串起两层；生产适配只依赖标准 `http`、`http_body` 和 Tower Service，不要求 Axum。A 也服务于静态、代理、原生 handler，不能放进 wasm feature 内。

已有 dhttp 顶层稿仍描述 component `apis()`；与当前 OpenAPI 决定冲突的部分须在上游同步，不能把它当成本适配层的前置要求。通用 WASI HTTP 导出及 imports 仍需校验。

## 2. 需要保留的结构

避免复制另一套 Request/Response，也不为每个 wasm 创建独立沙盒。所需结构主要是复用 Wasmtime 类型及少量内部执行状态：

| 结构 | 生命周期 / owner | 保存内容 |
| --- | --- | --- |
| Engine / Linker | 进程级运行设施 | 编译配置、宿主接口定义；不捕获某个身份的权限 |
| WasmApp | 不可变代码版本，可 clone | Engine、已编译 Component；可选缓存预实例化结果 |
| 身份沙盒账户 | Pishoo 按 Server 持有 | 按 App 选择的私有目录能力、身份总预算、身份取消信号、固定 Endpoint |
| StoreData | 单次 Store 持有 | ResourceTable、WasiCtx、WasiHttpCtx、请求能力、limiter、出站 hook、取消信号 |
| 执行记录 ExecutionRecord | 既有任务跟踪器持有至回收 | guest 任务结果、host 子任务、deadline、额度 lease、输入/输出终态 |
| 跟踪 Body 包装 | 每个请求/响应 body | 原始 Body、字节计数、EOF/error/drop 通知；不持有可并发访问的 Store |
| 传输完成句柄 | A 层持有 | 本次输出正常发送/失败/被替换的结果；与 guest Body 的终态分开 |

上述名字仅说明内部职责，优先用现成任务集合、取消令牌及 RAII guard 实现，不增加公共 Runtime/Registry 框架。配额总账户按身份归属，持久数据按 App 隔离；AppId 用于选择代码及私有目录、日志和取消某组件任务。

### 2.1 单次 Store 状态

测试中的 `ServerState` 实际每请求创建，这里统一命名为 `StoreData`，表示 `Store<T>` 中宿主自定义的 T，中文称“Store 宿主数据”。每请求新建是本项目的生命周期选择，不放进类型名称中。Wasmtime 官方 WASI HTTP handler 的 `WorkerState` 也将这一关联类型命名为 `StoreData`，示例中则有 `MyState`；没有唯一强制名称。本次仅参考命名，不引入新版 handler 的 worker/p3 执行模型。参见 [官方 handler 源码](https://docs.wasmtime.dev/api/src/wasmtime_wasi_http/handler.rs.html) 和 [WasiHttpView 示例](https://docs.wasmtime.dev/api/wasmtime_wasi_http/trait.WasiHttpView.html)。

伪代码：

```rust
// 示意字段；HostBindings / CompositeLimiter 等名字尚未冻结。
struct StoreData {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    host: HostBindings,          // 当前身份、已批准能力、固定出站 Endpoint
    limiter: CompositeLimiter, // 请求额度 + 身份/全局聚合账户
    cancel: CancellationToken,
}

// 与测试一样，实现 WasiView 和 WasiHttpView：
// 两者访问同一个 table，http view 返回受控出站 hooks。
```

Store、Instance 只由 guest 执行任务驱动，不把 `&mut Store` 交给网络发送任务。资源句柄通过 `ResourceTable` 管理；guest 处理 bytes 时经绑定代码进入线性内存，Request/Response 不以宿主 Rust 指针传入。

### 2.2 编译缓存与宿主环境

组件在发布准备阶段有界编译并验证，不在每次请求中编译。Linker 注册通用接口，实际目录、出站与额度从 StoreData 取得；目录由宿主按已验证的 Server/AppId 选择，仅预打开 `apps/<AppId>/data/` 为 `/data`，App 根目录中的 app.wasm/openapi.json 仅供宿主加载，不授予父目录或其他 App 的句柄；不可缓存带身份权限的实例。imports 超出支持范围时拒绝加载，不能补一个无限权限默认实现。

测试使用 `Engine::default()`、空 WasiCtx、默认 HTTP hooks，这些仅验证通路。生产必须启用实际 limiter、fuel/epoch 中断及宿主 I/O deadline，且在实例化前生效；初始化也可能执行 guest。目录能力、磁盘/handle/log/host buffer 计额不是 Store 内存 limiter 自动提供的。

## 3. 入站请求转换

### 3.1 h3x → 标准 HTTP Body

测试的核心组合为：

```rust
let (mut parts, body) = h3_request.into_parts();
let trailers = parts.extensions.remove::<Trailers>();
// ReaderStream 把 AsyncRead 变成分块字节流。
// 每块字节映射为 Frame::data，再封装成 StreamBody。
```

生产建议用小状态机包装，避免测试中简单 `chain` 在读错误后继续构造正常 trailers：

```text
ReadingData
  ├─ 有数据 → Frame::data(bytes)，继续
  ├─ Pending → 保留 waker，不主动无界预读
  ├─ 正常 EOF → TrailersPending
  └─ 读取失败 → 发出一次 Body error → Done
TrailersPending
  └─ 获取最终 trailers，非空则发出一次 Frame::trailers → Done
Done
  └─ 不再产生 frame
```

[RFC 9114 §4.1](https://www.rfc-editor.org/rfc/rfc9114.html#section-4.1) 规定 HTTP/3 消息中的顺序为初始 HEADERS、可选 DATA、可选 trailing HEADERS；它不规定 Rust Body/AsyncRead 的 EOF API。[h3x Trailers](../../h3x/src/common/trailers.rs) 另行约定：入站 body 正常 EOF 时 trailers 已完整。[读取实现](../../h3x/src/stream/read.rs) 先解码保存 trailers，再 shutdown body 缓冲。因此本适配器在正常 EOF 后取最终快照；收到初始请求头时不能假定 trailers 已到齐。提前读取共享句柄本身并非 RFC 禁止，只是不能把当时的内容当成最终结果。换成其他 Body API 时须遵循其约定，例如 trailers frame 可以出现在整个 frame 流 EOF 之前。多值 header/trailer 要保留，不可转成只支持单值的 map。应用未消费请求时由 A 层停止接收或有界回收，禁止为了清理无限排空上传。

入站 `Body` 使用 dhttp 约定的 `UnsyncBoxBody<Bytes, BoxError>`；只需 Send，不强加 Sync。正确保留 size_hint/is_end_stream，未知长度不能伪造 Content-Length。收到的数据、headers 和 trailers 均单独限量。

### 3.2 标准 HTTP → WASI HTTP resource

Pishoo 在此之前已完成路由、daccess 授权、`/api/<AppId>` 挂载前缀剥离及保留头清洗。B 层将标准 Body 的错误映射为 WASI HTTP ErrorCode，再调用测试中的真实接口：

```rust
let incoming = store.data_mut().http().new_incoming_request(
    scheme,
    http::Request::from_parts(parts, body),
)?;
let (sender, receiver) = oneshot::channel();
let outparam = store.data_mut().http().new_response_outparam(sender)?;
```

scheme 由可信传输决定，不能读取客户端伪造头切换。authority 必须已与目标 Endpoint 核对；path/query 保留应用约定，适配层不再二次解码。`HandshakeSummary`、SubjectId 等宿主 extensions 不会自动变为 guest 可见资源，只有批准的身份投影进入 headers。

`receiver` 只接收一次响应对象，不携带完整响应 body。body 自身是持续可消费的流，不调用 collect/read_to_end。

## 4. guest 调用与响应转换

### 4.1 启动和等待响应

```rust
// 伪代码：跟踪器必须在启动前登记 owner、额度及取消关系。
async fn call(app, request, prepared_host, execution_tracker) {
    let record = execution_tracker.register(prepared_host.lease, deadline);
    let (response_tx, response_rx) = oneshot::channel();

    record.spawn_owned(async move {
        let state = build_store_data(prepared_host);
        let mut store = Store::new(&app.engine, state);
        configure_limits_and_interrupts(&mut store); // 先于 instantiate
        let proxy = Proxy::instantiate_async(&mut store, &app.component, &linker).await?;
        let incoming = register_request_resource(&mut store, request)?;
        let outparam = register_response_outparam(&mut store, response_tx)?;
        proxy.wasi_http_incoming_handler()
             .call_handle(&mut store, incoming, outparam).await
    });

    // 同时关注响应、执行失败、deadline 和取消。
    // 任务先结束时仍检查响应通道是否已有值，避免 select 就绪竞态丢响应。
    let response = wait_response_or_failure(response_rx, &record).await?;
    wrap_body_and_return_response(response, &record)
}
```

不能先 await guest 完成再取响应，否则有界输出队列填满时 guest 等待读者、宿主等待 guest，形成死锁。guest 没有设置 outparam 就正常返回也属于执行错误，不能无限等通道。调用 future 被丢弃时同步发取消信号，跟踪器继续负责等待任务和释放额度。

### 4.2 响应先提交、body 后续流动

```text
guest ResponseOutparam::set
  → response_rx 得到状态/headers + Wasmtime 输出 Body
  → 标准 http::Response<B> 返回 Router
  → A 层开始写 HTTP/3 headers

guest output.write / OutgoingBody::finish
  → Wasmtime Body 的 data / trailers / EOF
  → A 层逐 frame 转发
```

guest 的 handler 返回、Body EOF 和 HTTP/3 writer 完成是三个不同事件。guest 返回后宿主可能仍有输出缓冲，Body EOF 后网络也可能尚未发送完成。所有剩余流资源都必须有明确 owner，不能按“响应头已生成”释放账户。

### 4.3 标准 Body → h3x 输出

测试采用有界 `ArcWndBuf` 连接两个并发 future：

```rust
let response = h3x::Response::from_parts(parts, ArcWndBuf::new(capacity));
let writing = writer.write_response(response.clone(), method, qpack);
let forwarding = forward_frames(body, response.clone());
// 生产需在任一侧失败时取消另一侧，然后等待两者终止；
// 正常路径并发驱动，不能顺序 await。
supervise_pair(writing, forwarding, cancellation).await
```

`forward_frames` 对 data 调用 `write_all`，对 trailers 逐值 `append_trailer`，正常 EOF 后 `shutdown`；trailers 必须在 shutdown 前设置。错误不能转换成正常 shutdown，必须使当前输出失败并唤醒对端生产者。测试使用 `ArcWndBuf::new(8)` 和 2ms sleep 刻意制造背压，生产不能照搬这些调试参数。

所有队列按字节有界并计入宿主资源预算。背压为：网络慢 → h3x 缓冲满 → forwarding 等待 → Wasmtime 输出队列满 → guest 等待可写。分块转换可能复制数据，流式不承诺零拷贝；流量限制不要求收集完整 body。

## 5. 生命周期、取消与错误

执行记录分别跟踪 input、guest、host operations、原响应 body 与最终 transport output 的结果，不用单个 bool 代表完成。额度在所拥有资源真正释放后返还；先释放的 Store 内存可以先归还内存份额，请求并发 lease 要保留到完整终态。

### 5.1 正常与取消

```text
准入 / 登记 → 实例化 → guest 运行
                      ├─ 响应就绪 → Body 转发 → 实际输出终态
                      └─ guest 终态 + host 子任务终态
任意阶段取消 → 停止相关 I/O、唤醒流等待、请求 guest 中断 → join 回收
所有分支终止 → 释放剩余额度，撤销执行记录
```

- 请求 reset、连接断开、deadline、身份删除和组件取消都进入幂等清理；仅单请求错误不能主动关闭共享连接。
- 取消 token 本身不会打断纯计算死循环。必须使用 fuel/epoch，宿主独立推进 epoch；Tokio timeout/abort 也不能替代 WASM 中断。阻塞宿主操作需自己的可取消实现。
- Drop 仅同步标记状态、发取消信号，不启动无人持有的异步清理。任务跟踪器持有 JoinHandle，最终 join；仅在 Response extensions 保存一个句柄会被中间件替换/丢弃，不能作为唯一 owner。
- response body drop 通知“原输出不再消费”，并不直接等于“取消最终 HTTP 响应”；实际发送层另报最终输出结果。

### 5.2 必须处理的特殊路径

| 情况 | 行为 |
| --- | --- |
| 编译或 imports 不合法 | 发布准备失败，旧 Router 不变 |
| 实例化/guest 在提交响应前失败 | 生成受控 500；超时按约定 504；释放输入和子任务 |
| outparam 返回错误/未调用即退出 | 不无限等待，映射为受控响应错误 |
| 已返回响应但尚未发送 headers | 由传输层明确提交状态决定能否替换，B 层不猜测 |
| headers 已发送后 guest/body 失败 | 无法改成新的状态码；失败当前 body/请求流，不伪造正常 EOF |
| 对端停止读响应 | 停止 forwarding、使 guest 输出失败、取消剩余操作并 join |
| guest 提前响应但继续读取上传 | 同时驱动两边，不因响应已提交而停止输入 |
| guest 已结束但上传未读完 | 有界停止/回收上传，不无限 drain |
| HEAD / 204 / 304 | 传输层按 HTTP 语义禁止发送 body；停止不被消费的原输出并回收 guest，不误 reset 合法无 body 响应 |
| middleware 替换响应 body | 取消原 body 的生产与相关任务，最终替换响应仍可正常发送 |
| Body EOF 后 guest 仍继续执行 | 保留追踪和 deadline，不让该任务逃逸配额或变成后台任务 |

原 body 被主动抑制/替换导致的预期 guest 写失败，应归类为回收结果，不污染已决定的最终响应。若此时仍需 guest 读取上传，协调器应明确保留该输入阶段并设置 deadline，不能在 body wrapper 的 Drop 中无条件杀掉整个请求。

错误分类至少区分取消、超时、准入/配额、guest trap、WASI 协议错误、输入/输出传输错误和宿主内部错误。日志记录细节，对外不回传完整内部路径或 trap 字符串。计量记录 Server/App/API、各阶段耗时、字节数和终止原因，不记录 body。

## 6. 出站 hook 与策略接缝

guest 使用标准 `wasi:http/outgoing-handler` 构造请求，不依赖 DHTTP SDK，也不能选择 DHTTP 源身份。`Linker` 注册 WASI HTTP 接口；每次请求的 `StoreData` 在 `WasiHttpCtxView` 中提供本次专用的 `DhttpHooks`。Wasmtime 的 `WasiHttpHooks::send_request` 是接收普通 HTTP 请求并替换默认发送行为的接缝；不能留下会经默认 HTTP 客户端出网的路径。下列代码只说明职责，不是实际 trait 签名：

```text
guest: 构造 WASI HTTP Request(method, scheme, authority, path, headers, body)
       → outgoing-handler.handle(request)
       → 等待 WASI HTTP Response(status, headers, body)

DhttpHooks.发送(request, 本次调用上下文):
    # 上下文由宿主在入站握手后创建：server=Alice、caller=Bob、endpoint=Alice
    # guest 只能给出普通 HTTP request，不能填写或修改这三个可信字段。
    检查 caller == server                 # 当前首版策略：Bob 调用时拒绝出站
    解析并规范化 scheme、authority、path     # 不接受 guest 选择源 Endpoint/证书
    检查 App 能力、目标/method allowlist、管理目标禁区及额度
    删除 guest 伪造的 pishoo-* 等保留身份头
    dreq = Alice 的 Endpoint 创建请求(method, 目标 URI, headers)
    # 若准许发送，远端看到的 DHTTP 源身份始终是 Alice，不会变成 Bob。
    并发执行：WASI 请求 body/trailers → dreq；dreq 响应 → WASI 响应 body/trailers
    将发送和接收任务登记到本次 ExecutionSupervisor
    遇到取消、deadline、超额或传输错误：停止两侧并回收，再返回 WASI HTTP 错误
```

这是普通 HTTP 消息到 DHTTP 传输的适配，不把 DHTTP API 暴露给 guest，也不启动独立 TCP/Hyper 客户端。DHTTP 返回 3xx 时不盲目代替 guest 跟随；guest 若再发一个请求，重新进入同一 hook 并检查新目标。`Endpoint` 的现有客户端接口可设置 method、URI、headers/body/trailers，读取 status、headers、body/trailers；WASI 与 DHTTP 之间的流式 Body 转换和完成通知仍需实现，不能假定现成类型直接互换。

每次 outgoing 都使用当前 invocation 的固定 Endpoint、批准能力、目标/method allowlist、字节/并发预算、deadline 与取消令牌；测试 `WasiHttpView` 的默认 hooks 不能直接用于 Pishoo。

Pishoo 提供策略，B 层确保 guest 的所有网络 imports 经受控实现；不开放可绕过 hook 的原生 socket。出站清除保留身份头，重定向后的目标重新检查，不能允许组件选择其他身份或证书。实际网络仍由 dhttp 执行，不由适配层另建带环境权限的 HTTP 客户端。

首版 guest 不能经 outgoing、自调用或管理代理修改 ACL、决定审批或借联系人操作间接授权。这个拒绝不能被普通 allowlist 或 owner 入站身份覆盖。检查实际目标的规则来自 Pishoo，而不是 dhttp 硬编码业务路径。

需要的接缝用职责描述，暂不冻结复杂公共 trait：

1. 加载时取得 Engine/Linker，验证 HTTP 导出与允许的 imports。
2. 请求执行前注入可信宿主上下文、目录能力、共享预算与取消关系。
3. 给真实 Store 安装 limiter、中断设置和受控 outgoing hooks。
4. 向既有请求任务跟踪器报告 guest/host I/O/Body 状态。
5. A 层独立报告实际输出完成、失败或被替换，必要时控制输入/输出半流。

缺失必需宿主设置应拒绝执行，不能默认提供全权限环境。普通独立 WASI HTTP 测试可使用显式空能力配置，不将其冒充生产身份沙盒。

## 7. 从 h3x 测试迁移的清单

| 现有测试/结构 | 可复用的事实 | 尚未覆盖或需修改 |
| --- | --- | --- |
| WasmHandler | Engine + Component 复用 | 有界编译、版本快照、 imports 校验 |
| ServerState | 每次新建 ResourceTable/WasiCtx/WasiHttpCtx | 注入共享身份账户、目录、limiter、hooks |
| handle_wasm | new_incoming_request / outparam / Proxy 调用 | 去 unwrap、响应与退出竞态、超时和 drop |
| GuestTask | 响应提交与 guest 完成分离 | 唯一 owner 移到任务跟踪器，body/网络分别报告 |
| serve | AsyncRead ↔ Frame Body ↔ h3x writer | 读错误后不发正常 trailers，首错取消另一侧，无 body/替换场景 |
| streams_large_bodies_and_trailers | 多块输出、重复 trailers、背压链路 | 示例数据仅相对很小的窗口较大，不证明大文件峰值内存有界 |
| sends_headers_before_request_body_finishes | 请求未结束时先发响应头 | guest 使用 read_to_end，不证明逐块业务处理 |
| cancelling_response_unblocks_guest | guest 在输出等待中被取消 | 不证明 CPU 死循环和全部宿主 I/O 可取消 |
| adapter_body_failure_cancels_output_without_poisoning_connection | 原生测试 Body 出错后同连接仍可服务 | 不等于已覆盖 guest trap、HEAD 和 Body 替换 |

对应 fixtures：[读完后响应](../../h3x/tests/fixtures/wasi-http-handler/handlers/read-request-then-respond/src/lib.rs)、[先响应再读请求](../../h3x/tests/fixtures/wasi-http-handler/handlers/respond-then-read-request/src/lib.rs)、[持续写直到取消](../../h3x/tests/fixtures/wasi-http-handler/handlers/stream-response-until-cancelled/src/lib.rs)。

实施先提取 A 层有界 Body 桥接，迁移已有四类行为测试；再提取 B 层真实 WASI 调用，接入请求任务跟踪和错误处理；最后接入身份共享预算、能力与 Pishoo 路由授权。不要把测试的 Axum router 或私有 fixtures 变成 dhttp 公共依赖。

## 8. 验收要求

- 保留上述四类测试行为，并覆盖输入中途出错时无正常 trailers/EOF。
- guest 提交响应与退出同时就绪不丢响应；未提交直接退出及时失败。
- 大上传/下载使用逐块 guest fixture，慢消费者情况下峰值缓冲受配置约束。
- headers 前/后 trap、deadline、CPU 死循环、host I/O 等待及调用 future 被丢弃均回收任务与额度。
- HEAD/204/304、middleware 替换 body、早响应继续上传不死锁、不误 reset 最终响应。
- 同身份多组件及旧新版本共用配额；同身份不同 App 的 data 不互通，不同身份的 data/上下文不串用。
- 未授权请求不实例化 guest；伪造保留头无效；宿主配置的 App 只读/出站限制及 guest 禁止管理 ACL 生效。
- 所有完成路径的 guest 任务、输入/输出、子请求、句柄和额度计数归零，单流失败不影响同连接其他流。

本次仅据代码编写文档并做链接/格式检查，没有修改 h3x 或 dhttp，也没有重新运行上述测试。上游公共签名与完成通知仍需按 [接入说明](dhttp-integration-contract.md) 对齐。

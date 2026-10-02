# Pishoo 第一版结构与接口清单

本清单遵循[设计准则和接口约束](README.md)。范围是 HTTP 网关、WASM、既有 daccess 集成及[exec 接缝](exec-interfaces.md)。这是设计，不表示实现已完成。

## 1. 第一版边界

- Pishoo 使用 dhttp Endpoint、本机 HTTP/1.1 客户端、标准 HTTP/Body、Tower/Axum；不持 QPACK、H3 writer 或 QUIC connection。
- Endpoint 独立 load；同名 Endpoint 经全局 Network 的同一本端身份池复用连接。Endpoint 不提供 close 或 stop_listening；Network 属于进程生命周期，不提供 shutdown。
- Server 串行加载和重载。没有 ServerState、Release、revision、构建队列或后台发布任务。
- WasmRuntime 只保存 Engine/Linker。编译按当前调用顺序执行，不建立编译任务注册表或并发槽。
- 配置反代只连接本机 HTTP/TCP 上游；同名身份专用的 DHTTP 正向代理使用 Server 现有 Endpoint。Lib 出站暂不实现，WASI HTTP 出站请求一律拒绝。没有 UpstreamKind、传输选择字段或失败回退。
- `tcp-mock` 是用户批准的测试构建例外：Server.listen 仍调用 Endpoint.listen，独立进程中的测试客户端使用 Endpoint 请求；dhttp 后端用回环 TCP 流承载 h3x 双向与单向流。测试客户端经过 dhttp/h3x，反代上游使用普通本机 HTTP/TCP。此路径不验证 QUIC、TLS 对端认证或路径发现。
- 一个 `.wasm` component 文件就是一个 Lib，不另设 App 概念。接收和响应复用下述标准 Body 别名。局部流转换不是新的模块接口。

```rust
type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;
```

除 run、validate_lib、Error 外，下文自有类型和模块接口均为 crate 内部；成员只在本crate所需模块间可见，不作为仓外API。字段列表完整，不增加通用状态map或预留成员。错误沿调用结果、Body或现成任务结果通道传播；互斥结果使用enum/Result，不在有效资源旁重复保存错误标记。

### Rust 文件组织

2026-09-26 用户要求使用普通 `mod` 声明和同名 `.rs` 文件，合并过碎的 Sandbox 实现。库根文件为 `src/pishoo.rs`；Sandbox 保留四个文件：

| 文件 | 职责 |
| --- | --- |
| `sandbox.rs` | 已冻结结构、组件加载与重载、关闭和 API 路由 |
| `sandbox/runtime.rs` | WasmRuntime/Lib 构造、Store 限制、Invocation 执行和响应体生命周期 |
| `sandbox/host.rs` | WASI HTTP 出站拒绝接缝和 identity WIT 宿主能力 |
| `sandbox/manifest.rs` | 组件 OpenAPI 清单校验 |

子模块为私有模块，既有对外路径通过 `use` 重导出。`Server` 的现有方法使用 `pub(super)` 供 daemon 内部调用；现有无状态函数 `static_file`、`proxy_uri`、`clean_hop_headers`、`workspace` 的可见范围限定在各自所属的 routes/sandbox 内。内部跨模块函数 `routes::reserved(path: &str) -> bool` 统一检查 `/contact`、`/contacts`、`/acl`、`/workspace`、`/workspace-api`、`/chat-api`、`/std`、`/api`、`/.pishoo`、`/exec`、`/file` 的路径段前缀。`clean_hop_headers` 只清理逐跳头及 Connection 点名的头；不保留或过滤 `pishoo-` 头前缀。可信身份只取自 request extensions 的 HandshakeSummary。exec 模块接口见[exec 清单](exec-interfaces.md)。

## 2. 配置和固定默认值

```rust
struct ServerConfig {
    listen: u8,
    exec: bool,
    proxy_locations: Vec<ProxyLocation>,
}
struct ProxyLocation {
    location: String,
    proxy_pass: http::uri::Parts,
}
```

没有实例配置文件或实例数据库。`run` 从启动时的 DHTTP_HOME 取得实例目录，不在运行中修改进程环境。身份通过 dhttp-home 发现，Endpoint 使用相同身份目录。每个 Server 从自己的 `db/config.db` 读取配置；不定义实例配置结构。单命令 exec 由本 Server 的 `exec` 开关控制，`exec=0` 禁用；`exec=1` 时只允许同名已验证远端身份。

config.db 的 schema v1 为 settings(listen,exec) 与 proxy_locations(location,proxy_pass)。settings 恰好一行；listen=0/1/2/3表示关闭/内网/外网/两者，exec 只能是整数0/1。proxy_pass 接受裸回环地址端口或其 `http://` URI，可带路径，不接受非本机目标；没有传输类型字段或数据库列。第一版尚未上线，直接修订 v1 建表定义，不添加 schema v2 或自动迁移。没有 lib_policies、policy_imports、默认策略来源账本。

2026-09-26 实施确认：用户批准将 `ProxyLocation.proxy_pass` 从 `http::Uri` 改为 `http::uri::Parts`。裸回环地址先补上 `http://`，再校验完整 URI；以 `path_and_query: None` 保留原始配置未写路径的事实，显式 `/` 则保存 `Some`。标准 `Uri` 会将两者规范化为相同值，无法落实既定的保留路径/替换前缀规则。不新增字段或自有结构。

以下运行约束不是配置字段：WASM 无总执行时长限制；每次调用 fuel100_000_000、每10_000 fuel让出。StoreLimits 使用 Wasmtime 47.0.4 默认值：每个 Store 最多10_000 instances、10_000 memories、10_000 tables，对每个 linear memory 的字节数和每张 table 的元素数不另设上限。WASI输出1块、每块16KiB；签名输入1MiB、签名8KiB。每个 Server 的退出等待上限15秒。exec 使用[exec 清单](exec-interfaces.md)的固定限制。

全局 Network 无初始化配置，直接调用 `DhttpNetwork::init()`。每个 Server 的 listen 范围在 `Endpoint.listen` 时交给 dhttp，Network 根据当前监听登记管理接口绑定。Pishoo 的 listen 配置变化仍在重启后生效；Lib 和路由重载不改变监听。

## 3. 运行循环和 Server

```rust
struct Server {
    profile: dhttp_home::identity::IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    access: std::sync::Arc<access_control::AccessService>,
    workspace: std::sync::Arc<Workspace>,
    chat: std::sync::Arc<Chat>,
    router: std::sync::Arc<std::sync::RwLock<axum::Router>>,
    sandbox: Sandbox,
    exec_tasks: tokio_util::task::TaskTracker,
}
```

```rust
pub async fn run() -> Result<()>;
pub fn validate_lib(bytes: &[u8]) -> Result<oas3::OpenApiV3Spec>;
fn load_server_config(profile: &dhttp_home::identity::IdentityProfile) -> Result<ServerConfig>;
impl Server {
    async fn load(profile: dhttp_home::identity::IdentityProfile,
        runtime: std::sync::Arc<WasmRuntime>) -> Result<Self>;
    fn name(&self) -> &str;
    async fn reload(&mut self) -> Result<()>;
    fn listen(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output=Result<()>> + Send + 'static>>;
    async fn close(&mut self) -> Result<()>;
}
fn file_router(root: std::path::PathBuf) -> axum::Router;
async fn proxy_pass(proxies: Vec<ProxyLocation>,
    request: http::Request<axum::body::Body>) -> axum::response::Response;
async fn forward_dhttp(endpoint: dhttp::Endpoint,
    request: http::Request<axum::body::Body>) -> axum::response::Response;
```

`run` 以局部变量持有 home、Server 集合和共享 WasmRuntime。启动时加载一次；Unix 上收到 SIGHUP 后才扫描身份并串行处理 Server 加载、重载和删除，不定时轮询。监听任务启动后不保留 JoinHandle；监听错误仅记录日志，不自动关闭或移除 Server。退出时逐个关闭并回收 Server。每个 Server 直接持有 exec 任务跟踪器；`exec` 与 listen 的变更在重启后生效。Server.listen在返回future前克隆Endpoint和router，不借用Server；因此监听运行期间仍可 `reload(&mut self)`。

每次请求仅短暂read-lock并clone当前Router，然后释放锁再驱动oneshot。Sandbox 构造的 Lib handler 捕获本路由的 Arc<Lib>、任务跟踪器与 Endpoint，不捕获 Server 或 Sandbox；普通 routes 模块不负责 WASM 执行。旧请求保有旧Router/Lib，不需要另一个发布对象。

reload 先读取配置，再调用 Sandbox.load_libs 扫描并更新 Lib 集合。Server 中显式合并管理、Lib API、exec 与静态文件 Router，再配置代理 fallback，并在完整 Router 外添加 daccess 授权层。静态文件仅在 `/file/{*path}` 提供，`/file` 本身不提供文件；代理 fallback 仅在命中配置的精确路径或路径段前缀时转发，否则返回 404。Lib 扫描或编译失败时直接返回错误，保留旧 Router、Lib 集合和配置。加载成功后构建完整 Router，一次替换，并更新 Server.config；其间没有 await 或可失败操作。没有部分挂入路由的中间状态。

`/.pishoo/dhttp/{*path}` 在代理 fallback 之前挂载；通配部分必须包含目标名称，支持无斜杠和带末尾斜杠的目标根路径及其子路径。目标名称来自单个路径段，规范化为 DHTTP 名称，可带证书序号；剩余原始路径与 query、方法及 Body 交给现有 Endpoint 发送。该入口除统一 daccess 授权外，要求已验证远端与当前 Server 同名且 SKI owner_hash 相同；不转带入站可信身份 extensions，清理逐跳头，并将目标设为 Host。响应状态、普通头及 Body 流式返回。输入无效返回400，身份不符返回403，DHTTP 出站失败返回502。它不修改本机 TCP 代理、Lib 出站或 Server 字段。


统一 DHTTP 正向代理继续只转发请求。2026-10-02 用户确认审批和联系人以远端目标分支为准：联系人申请改由 Workspace 的 `/workspace-api/contact-requests` 入队，按 application_id 向对端 `/contact` 投递并查询 `/contact/self`。ContactNotifier 与本地 `POST /contact/{name}` 接缝删除。生产出站适配按用户要求暂缓，现阶段保留分支的 OutboundTransport trait 与业务队列，尚未装配实际传输实现。

组件一次读出的bytes同时用于OpenAPI、摘要和编译，不在提交前重读文件。没有后台编译结果，也没有跨任务revision检查。删除与替换只由该actor执行。

新版本保存本版本目录权限；删除 Lib 时移除路由。旧请求仍持有的旧版本继续执行，直到自身结束。删除 Server 时调用 Server.close 并保留已关闭的 Server 记录；没有监听停止接口，原监听仍使用其已清空的 Router。同名身份重新出现须重启进程。

静态文件每请求固定已打开句柄。部署用临时文件+原子rename，不原地改写正在读取的文件；此为部署约束，不宣称整个file目录是不可变发布快照。

## 4. daccess、Workspace 与 Chat

2026-10-02 用户要求先 rebase 适配，并确认审批和联系人协议以远端目标分支为准。依赖固定为 `daccess/feat/fit-pishoo@cf8f72f4e6bedbd7c98648ffee31053cb509b395`；Workspace 和 Chat 的功能基线为 `pishoo/feat/daccess@9b733c52008898c0921e9c01e05533b7302600ad`。以下规则替代此前请求内等待审批和 ContactNotifier 回调约定。

每个 Server 加载 profile 的 `db/access.db`，以名称及已验证本端证书 SKI 的 owner_hash 文本字节初始化 AccessService。可信来访身份取自 HandshakeSummary；先清除请求携带的 Visitor，再从 remote 的证书构造 Visitor。缺少握手或本端身份仍是接入错误，无效 SKI 拒绝，remote=None 才是匿名。

```rust
async fn authorize(
    axum::extract::State(access): axum::extract::State<std::sync::Arc<access_control::AccessService>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response;
fn access_router(access: std::sync::Arc<access_control::AccessService>) -> axum::Router;
```

Headers 只含 method、完整 path_and_query 和 fields。RequestId 由 daccess 内部派生；Pishoo 不生成、持有或转交外部 request ID。Allowed 进入业务，Denied 返回403，Reviewing(PendingReview) 立即返回202及库定义的 PendingReviewResponse。查询获准后，由来访者重试相同请求，库消费一次性决定；断开请求不删除持久审批。匿名命中 review 时拒绝。

Visitor 构造后，库的 `is_review_status_path` 和 `is_contact_status_path` 指定的 GET 端点直接交给库 handler，按名称和 SubjectId 校验记录归属。公开资料只对 GET `/std/profile` 与 `/std/profile/avatar` 放行。其余入口仍受统一 daccess 授权。Workspace/Chat 本地管理 handler 再核对 owner；POST `/std/message` 在 daccess 允许后检查 capability decision 的联系人、SubjectId、请求编号和版本。

管理 Router 直接使用 `access_control::management_router`：联系人采用 application_id、接收方起算7天期限及申请方轮询；审批管理为 `/acl/reviews`、PATCH `/acl/review` 和 `/acl/review/{id}/status`。删除 DhttpContactNotifier、submit_application、granted_update、管理路由 Endpoint 参数，以及本地回调协议。申请接收仍遵循接收方自身的 daccess 策略。

Server 直接持有 Workspace 和 Chat 的 Arc；资源、数据库及路由清单见 [Workspace/Chat 接入清单](workspace-chat-interfaces.md)。这次保留目标分支现有业务模型和 worker 资源，每个模块使用自身已有的句柄与关闭信号；不额外添加 Server 投递任务集合。reload 复用同一资源和队列，close 清空 Router 并停止、等待两个模块的 worker。

Workspace 以普通 `workspace.rs` 模块组织；Chat 使用 `chat.rs`。完整前端位于 `pishoo/workspace`，由 Bun 构建并以内嵌 dist 提供。`/workspace` 返回307，深链接回到 index.html，缺失资源404，GET/HEAD 沿用当前静态入口。`/workspace-api/context` 采用分支的 profile、owner_name、badges 响应，并要求 owner。

Lib API 仍不自动写入默认访问规则。Lib 的 WASI HTTP 出站继续拒绝；现有本机反代及 DHTTP 通配转发保持各自职责。底层出站与真实网络测试按用户要求暂缓，不新增预期 SubjectId 的底层接口。

## 5. Sandbox、组件管理与执行隔离

```rust
struct Sandbox {
    libs: std::collections::HashMap<String, std::sync::Arc<Lib>>,
    runtime: std::sync::Arc<WasmRuntime>,
    tasks: tokio_util::task::TaskTracker,
}
impl Sandbox {
    fn new(runtime: std::sync::Arc<WasmRuntime>) -> Self;
    fn load_libs(&mut self, profile: &dhttp_home::identity::IdentityProfile)
        -> Result<()>;
    fn api_router(&self, endpoint: dhttp::Endpoint) -> axum::Router;
    fn close(&mut self);
    async fn wait(&self) -> Result<()>;
}
```

每个 Server 直接持有一个 Sandbox，集中管理该身份的 Lib 集合、共享 WasmRuntime 引用与任务跟踪器。`new` 接收 `run` 创建并跨身份共享的 WasmRuntime，创建空 Lib 集合和空 TaskTracker。重载在原 Sandbox 上串行进行，不需要内部锁。Lib 执行不限制并发数，不设置执行槽或 permit。静态、代理和 exec 不登记到该 TaskTracker；exec 使用 Server 自己的 TaskTracker。

`load_libs` 扫描并串行编译局部候选，摘要未变时复用原 Arc；任一 Lib 加载失败直接返回错误，不修改当前 Lib 集合。完整扫描成功后更新自身集合。局部候选只是调用栈中的标准 HashMap，不引入构建会话、候选容器或发布状态。

`api_router` 根据 Sandbox 当前 Lib 集合构造 `/api` 分支，负责 Lib 查找、声明路径与方法验证、请求 URI 处理和 Invocation 执行。它只克隆本版本需要的 Lib 与共享执行资源；完整对外路径上的 daccess 授权由 Server 组装 Router 时统一添加，Server 保留完整 Router 的发布权。

`close` 同步关闭 TaskTracker 并清空集合，不等待任务。`wait` 在固定15秒内等待已关闭的 TaskTracker，超时返回 ShutdownDeadline。调用方 Server 清空 Router 并调用 Sandbox.close，再调用 `wait` 回收 WASM 任务。Sandbox 不持有自己的取消信号、身份、Endpoint、策略或派生计数；它不是独立的操作系统进程或容器。

2026-09-26 用户明确确认将 WASM 职责集中到 Sandbox：Server 的 `libs`、`runtime` 迁入已有 Sandbox，`sandbox` 改为直接所有；组件加载、替换和 API Router 方法归 Sandbox，构造与关闭签名相应调整。当时 `build_router` 接收标准 axum::Router，不再接收 Lib 集合或 Sandbox；该函数后来由 Server 中的显式 Router 组装取代。共享 WasmRuntime 由运行入口持有，Server 保留 Endpoint、授权、整体 Router 和 exec 任务跟踪器；当次迁移保持 Invocation 和 StoreData 的字段及调用签名；随后取消 Lib 并发限制的变更见下文。

2026-09-26 用户要求取消 Lib 并发限制：删除 `Sandbox.lib_slots`、`Invocation.permit`、`StoreData.permit`、`Invocation::new` 的 permit 参数及仅用于满额拒绝的 `Error::Capacity`。请求通过 daccess 授权、Lib API 匹配和可信身份校验后直接执行，不因在途执行数返回429。重载复用同一个 Sandbox，旧版本执行仍由同一个 TaskTracker 跟踪。2026-09-28 用户要求移除旧 Router、Invocation 构造和执行入口对 Lib token 与 TaskTracker 关闭状态的预先拒绝；TaskTracker 关闭状态只用于 `wait`，不阻止后续 spawn。

隔离由每次执行的独立 Store/Instance、受限 WasiCtx、Lib 私有 `/data`、默认 StoreLimits、fuel 和宿主能力控制完成。默认 StoreLimits 不对单个 linear memory 的字节数设额外上限；每次调用 fuel100_000_000。不累计多个 memory 或多个并发调用的内存，没有 Sandbox 总额度或总 fuel 预留，不另存派生计数或租约结构，也不等待传输FIN/ACK。

默认 StoreLimits 不提供总内存字节硬配额，也不限制 Wasmtime 的全部宿主分配或磁盘用量。

## 6. WasmRuntime、Lib 和 Store

```rust
struct WasmRuntime {
    engine: wasmtime::Engine,
    linker: wasmtime::component::Linker<StoreData>,
}
struct Lib {
    digest: [u8; 32],
    openapi: oas3::OpenApiV3Spec,
    component: wasmtime::component::Component,
    runtime: std::sync::Arc<WasmRuntime>,
    filesystem: wasmtime_wasi::filesystem::WasiFilesystemCtx,
}
struct StoreData {
    table: wasmtime::component::ResourceTable,
    wasi: wasmtime_wasi::WasiCtx,
    http: wasmtime_wasi_http::p2::WasiHttpCtx,
    memory: wasmtime::StoreLimits,
    deny_outgoing: DenyOutgoing,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
}
struct DenyOutgoing;
```

```rust
impl WasmRuntime {
    fn new() -> Result<Self>;
    fn compile(&self, bytes: &[u8]) -> Result<wasmtime::component::Component>;
}
impl Lib {
    fn load(runtime: std::sync::Arc<WasmRuntime>, id: String, bytes: &[u8], data_dir: &std::path::Path) -> Result<Self>;
}
```

Runtime直接编译，启动/重载串行调用；不另存compile_slots、compile_tasks、cancel。Engine启用component model和consume_fuel，禁guest threads。linker注册WASI HTTP与既有identity WIT，instantiate_pre验证imports；不执行guest读取manifest。

每个Lib只预打开自己的lib/<LibId>/data为可写的/data，再保存filesystem clone；禁止身份根、db、ssl和兄弟Lib。使用现有WasiCtxBuilder.preopened_dir，不发明cap-dir注入API。

2026-09-28 用户要求删除 `Lib.id` 字段。扫描阶段的目录名仍作为 `Lib::load` 的输入进行验证，并直接作为 `Sandbox.libs` 的键；Lib 实例不重复保存该名称。

不保存 LibPolicy 或 OutgoingRule。Lib 暂无出站能力；DenyOutgoing 是 Wasmtime 必需的无状态 hook，其 send_request 始终返回 HttpRequestDenied。签名和验证能力直接由身份宿主提供，不增加逐 Lib 策略表或权限热更新框架。

Sandbox、WasmRuntime、Lib、StoreData、Invocation 和 DenyOutgoing 同属 sandbox 逻辑模块，组件扫描、manifest 验证、WASI 执行及身份宿主能力实现均在该模块内按文件拆分；移除原 wasm 模块，不新增执行包装层。`validate_lib` 的公开根级导出保持不变。StoreData 实现现成 WasiView、WasiHttpView 及 identity WIT host；直接使用 Wasmtime StoreLimits 默认值，不再实现自定义 ResourceLimiter 或保存多memory合计计数。Store 在实例化前安装 limiter、fuel 和 fuel_async_yield_interval；TaskTracker 直接跟踪 guest 任务，不依赖 Body 下一次 poll。

2026-09-28 用户要求精简内存限制：`StoreData.memory` 改为 `wasmtime::StoreLimits`，删除 `MemoryLimits` 及其 `ResourceLimiter` 实现。当时每个 linear memory 仍限64MiB；不限制单次调用多个 memory 合计或 Sandbox 内并发调用合计。随后用户要求使用 `StoreLimits::default()`，撤销显式的单块内存、单表元素与 Store 资源数量限制，改用上文所述的 Wasmtime 默认值。

## 7. Lib 执行与响应体

```rust
struct Invocation {
    lib: std::sync::Arc<Lib>,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
    tasks: tokio_util::task::TaskTracker,
}
impl Invocation {
    fn new(lib: std::sync::Arc<Lib>,
        endpoint: dhttp::Endpoint,
        handshake: &dhttp::HandshakeSummary,
        tasks: tokio_util::task::TaskTracker) -> Result<Self>;
    async fn execute(self, request: http::Request<Body>) -> Result<http::Response<Body>>;
}
```

Lib 不保存取消 token。Sandbox.close 和 Lib 删除只撤销路由与集合引用，在途 guest 继续由 TaskTracker 跟踪直到自身结束；Body 丢弃不另发取消信号。Invocation::new 的 Endpoint 参数只用于核对握手本端身份，不保存到 Invocation。

TaskTracker 直接登记持有 Store 的 guest 任务，不设置 WASM 总执行期限，也不新增 watchdog、deadline 成员或通知接口。单次计算由 fuel 限制且周期性让出执行权。

调用登记并启动 guest；outparam 的 oneshot 收到响应后，`Invocation::execute` 直接将 Wasmtime 原生响应 Body 的帧错误适配为通用 Body 错误并返回。此时丢弃 JoinHandle 不取消 guest，TaskTracker 仍跟踪其生命周期。响应通道关闭且没有提交响应时，等待 JoinHandle 并优先报告 JoinError 或 guest 错误；任务正常结束却没有响应则返回 GuestExitedWithoutResponse。outparam 显式拒绝响应时返回 GuestRejectedResponse。

响应 Body 直接交付 Wasmtime 的 data、trailers、EOF 和流错误，不等待 guest 任务结束，也不另行传播任务在提交响应后的失败。Body 被提前丢弃时不主动取消 guest；guest 对已关闭 Body 的后续 I/O 可返回错误。没有后续 I/O 的 guest 可能继续运行，直到自行结束。

静态和代理直接使用现成 Body；文件、上传源和上游响应由各自的流对象持有，提前 Drop 沿既有适配停止对应 I/O。上传自然 EOF 不取消响应方向。HEAD/204/304、middleware 替换 body 时丢弃旧 Body，不另行取消 guest；最终合法响应由 dhttp 继续发送。不需要通用租约 Body。

## 8. 本机反代、DHTTP 正向代理、Lib 出站拒绝与身份签名

```rust
async fn proxy(route: ProxyLocation,
    request: http::Request<Body>) -> Result<http::Response<Body>>;
```

WASI HTTP 的入站处理仍需 WasiHttpHooks；当前依赖关闭了默认网络发送器。StoreData 持有无状态 DenyOutgoing，send_request 一律返回 HttpRequestDenied，不创建连接、子任务或取消信号。Lib 出站能力留待单独设计。

proxy 完成路径/query 与 authority 转换后，以 Hyper HTTP/1.1 客户端连接配置中的回环 TCP 地址；每个请求建立一条连接，不增加连接池状态。转发前按上游 authority 设置 Host、清理逐跳头，不自动生成 `X-Forwarded-*`。响应也清理逐跳头，Body 保持流式背压和错误传播；连接和响应头各有30秒期限，连接或响应头失败返回网关错误。

DHTTP 正向代理固定在 `/.pishoo/dhttp/` 前缀，不读取 proxy_locations，不接受本机 HTTP/TCP 目标，也不回退到配置反代。它使用本 Server Endpoint 的凭据；对端看到的是影子身份的证书，不是调用手机的证书。其请求和响应 Body 复用标准适配与流背压，不增加自有传输状态。

已有 `pishoo:identity/signatures@0.1.0` 的 sign/verify 保留。StoreData 检查固定输入上限；sign 直接使用 qtls::LocalAuthority 选择 DHTTP 规范签名算法。verify 只使用已验证的 local 或当前握手 remote 公钥；其他身份返回 Unavailable，不发起远端解析。使用 dhttp-home 的 verify_signature，不把私钥或 authority 交给 guest。

## 9. 启停、exec 与错误

启动顺序：读各 Server 的数据库配置 → 串行加载身份/AccessService/组件 → 初始化一次全局Network → 为每个需监听的 Server 启动 listen 任务。身份或 Lib 加载失败直接结束启动；静态身份没有代理行也可启动。

入口先验证 Server 身份；`/exec` 与 Lib 路由一并装入 Server 的 Router，经过统一 daccess 授权层后由 exec 模块额外检查同名已验证远端身份，并按[exec 清单](exec-interfaces.md)执行。Server.listen 不特殊分派 `/exec`。不存在交互终端、终端协议版本头或 TerminalManager。

Server.close 同步调用 Sandbox.close()、关闭 exec 任务登记并清除 Router，随后分别等待 Sandbox 与 exec 任务回收。`run` 不收回 listener 任务；删除身份只关闭应用执行，原监听仍登记直到进程退出，同名身份重新出现须重启进程。Server 不批量取消 HTTP、审批或已启动的 exec；已进入的 HTTP 请求由其自身生命周期继续处理。`run` 退出时逐个调用 Server.close。全局 Network 不提供 shutdown；其连接池和后台维护随进程退出结束。`run` 直接等待进程退出信号，不另存取消 token。

每个 Server.close 的等待使用固定15秒上限；`run` 不设置全体 Server 的退出期限。exec 任务在单次取消后仍持有 Child 直到回收；超时明确返回ShutdownDeadline，不谎称任务已join。编译同步执行不可由Tokio abort中断，v1串行重载期间停机可能等当前编译返回；不为解决这一点暗加后台编译框架。

```rust
pub enum Error {
    BadRequest(String), BackendUnavailable(String), InvalidConfig(String),
    InvalidComponent(String), InvalidIdentity(String), IdentityMismatch, MissingHandshake,
    RouteNotFound, MethodNotAllowed, Denied, Cancelled, Deadline, Closed,
    GuestExitedWithoutResponse,
    GuestRejectedResponse(wasmtime_wasi_http::p2::bindings::http::types::ErrorCode),
    Guest(wasmtime::Error), Io(std::io::Error), Database(sea_orm::DbErr),
    ConfigDatabase(rusqlite::Error), Http(http::Error), Dhttp(dhttp::Error),
    Task(tokio::task::JoinError), ShutdownDeadline,
}
impl Error { fn status(&self) -> http::StatusCode; fn body_error(self) -> dhttp::BoxError; }
```

Error实现Display/Error。业务拒绝在headers前生成HTTP响应；headers后只返回body error。Server/WasmRuntime/Invocation不同时保存正常资源和独立error标记；整体可用/失败结果可以直接用Result表达。exec 在生成响应前完成并回收子进程。

## 10. 验收边界

- 从Endpoint进入标准Service；Pishoo生产代码和测试驱动不传QPACK/H3写流。
- 串行reload一次换Router；失败保留旧Router；旧请求持旧Lib，删除 Lib 不取消在途执行。
- 按固定 daccess 分支验证允许、拒绝、202 持久审批、查询归属和一次性重试；验证联系人申请编号、轮询及幂等。Workspace/Chat 的本地管理要求 owner，公开资料仅放行精确 GET 端点。
- WASM提前响应继续上传、多值trailers、body替换、HEAD/204/304、超过4次并发执行及任务回收；Body丢弃不单独取消guest。
- 配置反代仅连接回环 HTTP/TCP 服务；同名身份的固定前缀 DHTTP 正向代理使用现有 Endpoint；Lib 的 WASI HTTP 出站一律拒绝。验证本机代理响应分块在上传 EOF 前到达，上传保持打开且模拟空闲31秒后仍可双向传输。
- 同名 Endpoint 共享本端身份连接池；Server.close 清空应用 Router，但不停止监听，同名 Server 恢复需重启进程；其他身份不因服务关闭而中断。
- exec 验证同名身份、输入输出限制、超时取消与子进程回收；本版不宣称文件或网络隔离。

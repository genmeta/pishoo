# Pishoo 第一版结构与接口清单

本清单遵循[设计准则和接口约束](README.md)。范围是 HTTP 网关、WASM、既有 daccess 集成。这是设计，不表示实现已完成。

## 1. 第一版边界

- Pishoo 使用 dhttp Endpoint、本机 HTTP/1.1 客户端、标准 HTTP/Body、Tower/Axum；不持 QPACK、H3 writer 或 QUIC connection。
- Endpoint 独立 load；同名 Endpoint 经全局 Network 的同一本端身份池复用连接。Endpoint 不提供 close 或 stop_listening；Network 属于进程生命周期，不提供 shutdown。
- Server 仅启动时串行加载；身份、配置、Lib 和凭据更新统一重启，不支持 SIGHUP 重载或每日 OCSP 自动更新。没有 ServerState、Release、revision、构建队列或后台发布任务。
- WasmRuntime 只保存 Engine/Linker。编译按当前调用顺序执行，不建立编译任务注册表或并发槽。
- 配置反代只连接本机 HTTP/TCP 上游；同名身份专用的 DHTTP 正向代理使用 Server 现有 Endpoint。Lib 出站暂不实现，WASI HTTP 出站请求一律拒绝。没有 UpstreamKind、传输选择字段或失败回退。
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
| `sandbox.rs` | 已冻结结构、组件加载、关闭和 API 路由 |
| `sandbox/runtime.rs` | WasmRuntime/Lib 构造、Store 限制、Invocation 执行和响应体生命周期 |
| `sandbox/host.rs` | WASI HTTP 出站拒绝接缝和 identity WIT 宿主能力 |
| `sandbox/manifest.rs` | 组件 OpenAPI 清单校验 |

子模块为私有模块，既有对外路径通过 `use` 重导出。`Server` 的现有方法使用 `pub(super)` 供 daemon 内部调用；现有无状态函数 `static_file`、`proxy_uri`、`clean_hop_headers`、`workspace` 的可见范围限定在各自所属的 routes/sandbox 内。内部跨模块函数 `routes::reserved(path: &str) -> bool` 统一检查 `/contact`、`/contacts`、`/acl`、`/workspace`、`/workspace-api`、`/chat-api`、`/std`、`/api`、`/sys`、`/.pishoo`、`/file` 的路径段前缀。`clean_hop_headers` 只清理逐跳头及 Connection 点名的头；不保留或过滤 `pishoo-` 头前缀。可信身份只取自 request extensions 的 HandshakeSummary。

## 2. 配置和固定默认值

```rust
struct ServerConfig {
    listen: u8,
    proxy_locations: Vec<ProxyLocation>,
}
struct ProxyLocation {
    location: String,
    proxy_pass: http::uri::Parts,
}
```

没有实例配置文件或实例数据库。`run` 从启动时的 DHTTP_HOME 取得实例目录，不在运行中修改进程环境。身份通过 dhttp-home 发现，Endpoint 使用相同身份目录。每个 Server 从自己的 `db/config.db` 读取配置；不定义实例配置结构。不提供宿主命令 exec 入口。

config.db 的 schema v1 为 settings(listen) 与 proxy_locations(location,proxy_pass)。settings 恰好一行；listen=0/1/2/3表示关闭/内网/外网/两者。proxy_pass 接受裸回环地址端口或其 `http://` URI，可带路径，不接受非本机目标；没有传输类型字段或数据库列。第一版尚未上线，直接修订 v1 建表定义，不添加 schema v2 或自动迁移。没有 lib_policies、policy_imports、默认策略来源账本。已有 schema v1 中的旧 exec 列原样保留但不读取、不更新；配置 API 只返回 listen，PATCH 的 exec 字段作为未知字段拒绝。

2026-10-04 用户要求实施正常启动初始化：默认 home 使用现有 dhttp-home 的用户目录解析，目录布局保持 profile 根级 ssl/db/file/lib/logs/repo/templates，头像继续使用 assets/profile。Server.load 在凭据加载后创建缺失目录；load_server_config 首次创建 schema v1（listen=3，内外网均监听、空代理），已有配置校验后加载。新 access 由现有 daccess API 在临时库初始化并写入 POST /contact 的 Allow/Named 规则，经 SQLite 快照验证后发布；已有v1不补默认规则，原生v0先备份再由库事务升级。0.8.2旧ACL、未知格式、损坏和不支持版本拒绝启动并保留原数据。Workspace/Chat 在各自 open/migrate 内识别空库、校验当前版本和必要结构；不增加schema版本、表、类型、字段或跨模块函数。配置 API 读取不初始化缺失配置库；身份变化在下次启动处理。安装脚本不操作用户数据库，不恢复server.conf或实例配置。详情见配置 API 文档。

2026-10-03 用户批准配置 API 第一版：`setup::config_router(profile, endpoint)` 使用现有 H3 监听提供 `GET/PATCH /sys/settings` 和 `GET/PUT /sys/proxies`。前者读写 listen，后者读写整个代理规则数组；继续使用 schema v1。请求经过既有 daccess 授权层，处理器复核已验证 Visitor 与 Endpoint 同名、owner_hash 相同。JSON 只在请求内解析，不增加配置 DTO、字段或持久状态；SQLite 即时事务保证部分设置更新和代理列表替换的原子性。API 支持 Accept-Versions 的 v1 协商，响应 no-store。写入只更新数据库，代理和 listen 均需重启；接口不保证仅本地网络访问，当前握手信息不含网络范围。详见 [配置 API](../pishoo/docs/config-api.md)。

2026-09-26 实施确认：用户批准将 `ProxyLocation.proxy_pass` 从 `http::Uri` 改为 `http::uri::Parts`。裸回环地址先补上 `http://`，再校验完整 URI；以 `path_and_query: None` 保留原始配置未写路径的事实，显式 `/` 则保存 `Some`。标准 `Uri` 会将两者规范化为相同值，无法落实既定的保留路径/替换前缀规则。不新增字段或自有结构。

以下运行约束不是配置字段：WASM 无总执行时长限制；每次调用 fuel100_000_000、每10_000 fuel让出。StoreLimits 使用 Wasmtime 47.0.4 默认值：每个 Store 最多10_000 instances、10_000 memories、10_000 tables，对每个 linear memory 的字节数和每张 table 的元素数不另设上限。WASI输出1块、每块16KiB；签名输入1MiB、签名8KiB。每个 Server 的退出等待上限15秒。

全局 Network 无初始化配置，直接调用 `DhttpNetwork::init()`。每个 Server 的 listen 范围在 `Endpoint.listen` 时交给 dhttp，Network 初始化全部可用网卡并监听系统事件维护绑定；监听范围由 qconn 按名称执行，停止监听保留 socket。Pishoo 的配置、Lib、路由与凭据变化均在重启后生效。

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
    publisher: Option<std::sync::Arc<ddns::H3Resolver>>,
}
```

```rust
pub async fn run() -> Result<()>;
pub fn validate_lib(bytes: &[u8]) -> Result<oas3::OpenApiV3Spec>;
fn load_server_config(profile: &dhttp_home::identity::IdentityProfile) -> Result<ServerConfig>;
fn config_router(profile: dhttp_home::identity::IdentityProfile,
    endpoint: dhttp::Endpoint) -> axum::Router;
impl Server {
    async fn load(profile: dhttp_home::identity::IdentityProfile,
        runtime: std::sync::Arc<WasmRuntime>) -> Result<Self>;
    fn name(&self) -> &str;
    async fn listen(&self) -> Result<dhttp::ListenFuture>;
    async fn close(&mut self) -> Result<()>;
}
fn file_router(root: std::path::PathBuf) -> axum::Router;
async fn proxy_pass(proxies: Vec<ProxyLocation>,
    request: http::Request<axum::body::Body>) -> axum::response::Response;
async fn forward_dhttp(endpoint: dhttp::Endpoint,
    request: http::Request<axum::body::Body>) -> axum::response::Response;
```

`run` 以局部变量持有 home、Server 集合和共享 WasmRuntime。启动时串行加载一次；身份、配置、Lib 和凭据的变化均需重启，不注册 SIGHUP，不定时扫描或更新 OCSP。启动仍校验身份并在本地 OCSP 缓存缺失或无效时获取、验证和保存缓存。监听登记失败结束启动并统一收尾；登记成功交付 ListenFuture 后 spawn，不保留 JoinHandle。退出时逐个关闭并回收 Server。Server.listen 返回的 ListenFuture 不借用 Server。

每次请求仅短暂read-lock并clone当前Router，然后释放锁再驱动oneshot。Sandbox 构造的 Lib handler 捕获本路由的 Arc<Lib>、任务跟踪器与 Endpoint，不捕获 Server 或 Sandbox；普通 routes 模块不负责 WASM 执行。旧请求保有旧Router/Lib，不需要另一个发布对象。

Server.load 读取配置、加载 Endpoint 和 Sandbox Lib，显式合并管理、Lib API 与静态文件 Router，再配置代理 fallback，并在完整 Router 外添加 daccess 授权层。静态文件仅在 `/file/{*path}` 提供，`/file` 本身不提供文件；代理 fallback 仅在命中配置的精确路径或路径段前缀时转发，否则返回 404。Lib 扫描或编译失败直接结束启动；运行期间不替换 Lib、配置、Endpoint 或完整 Router，close 仍清空 Router。

`/.pishoo/dhttp/{*path}` 在代理 fallback 之前挂载；通配部分必须包含目标名称，支持无斜杠和带末尾斜杠的目标根路径及其子路径。目标名称来自单个路径段，规范化为 DHTTP 名称，可带证书序号；剩余原始路径与 query、方法及 Body 交给现有 Endpoint 发送。该入口除统一 daccess 授权外，要求已验证远端与当前 Server 同名且 SKI owner_hash 相同；不转带入站可信身份 extensions，清理逐跳头，并将目标设为 Host。响应状态、普通头及 Body 流式返回。输入无效返回400，身份不符返回403，DHTTP 出站失败返回502。它不修改本机 TCP 代理、Lib 出站或 Server 字段。

2026-10-03 用户决定底层出站仅保留 Empty/WndBuf，并批准 Pishoo 本轮先适配字节流、不支持正向代理的请求 trailers。该入口将标准入站 Body 的 DATA 逐块写入现有 RequestWriter，以有界窗口提供背压，EOF 后显式 shutdown；不全量缓存请求。声明 Trailer 头的请求在转发前返回400；流中出现未声明 trailers 时返回上传错误、记录日志并丢弃未完成 writer 以取消上传，不静默丢弃 trailers。若响应已交付，不能追溯改变响应状态。响应继续使用原生 Body，保留响应 trailers。等待响应头时取消请求会中止本次上传；交付响应后上传独立继续，不以响应 Body 额外控制上传。没有新增有状态结构、字段或跨模块接口。


统一 DHTTP 正向代理继续只转发请求。2026-10-02 用户确认审批和联系人以远端目标分支为准：联系人申请改由 Workspace 的 `/workspace-api/contact-requests` 入队，按 application_id 向对端 `/contact` 投递并查询 `/contact/self`。ContactNotifier 与本地 `POST /contact/{name}` 接缝删除。2026-10-03 用户要求接入生产出站；Server.load 为 Workspace 和 Chat 的既有 OutboundTransport 装配当前 Endpoint，Chat 使用已批准的发送前 owner_hash 校验，具体接口见 Workspace/Chat 与 dhttp 清单。

组件一次读出的bytes同时用于OpenAPI和编译，不在提交前重读文件。没有后台编译结果，也没有跨任务revision检查。运行期间不扫描或替换组件。

组件使用本版本目录权限；Lib 或身份目录变化在重启后生效。运行中不删除 Server 或替换 Lib；Server.close 在退出时清空路由并等待已有执行。

静态文件每请求固定已打开句柄。部署用临时文件+原子rename，不原地改写正在读取的文件；此为部署约束，不宣称整个file目录是不可变发布快照。


### DNS 解析、发布与自然过期

2026-10-03 用户批准；详细行为和跨仓接口见 [DNS 设计](pishoo-dns-detailed-design.md)。新增普通 `dns.rs`，只提供下列内部函数，不新增管理结构或 ServerConfig 字段：

```rust
fn install() -> std::io::Result<ddns::mdns::MdnsResolverSet>;
fn authority(endpoint: &dhttp::Endpoint) -> std::io::Result<qtls::LocalAuthority>;
fn publisher(endpoint: &dhttp::Endpoint) -> std::io::Result<std::sync::Arc<ddns::H3Resolver>>;
async fn maintain_mdns(mdns: &ddns::mdns::MdnsResolverSet,
    endpoints: &[dhttp::Endpoint], removed_bounds: &[std::net::SocketAddr]) -> std::io::Result<()>;
async fn publish(name: String, publisher: std::sync::Arc<ddns::H3Resolver>,
    addresses: std::sync::Arc<[dhttp::resolve::EndpointAddr]>) -> Option<tokio::time::Instant>;
```

run 在任何出站及 Network.init 前各注册一次现成 SystemResolver、匿名 H3Resolver 和共享 MdnsResolverSet；先订阅内外地址事件再初始化网络。listen 为 0/1/2/3 时分别不发布/仅 mDNS/仅 H3/两者，解析不依赖监听。外网监听 Server 的 publisher 绑定原 Endpoint，其余无该资源。监听身份在加载时校验 ClientAndServer 发布凭据。

run 仅用局部 FuturesUnordered 保存当前发布批，每个身份最多一个请求；每批读取地址簿的最新完整快照，批间消费地址事件与续期时刻。发布期限3秒，失败或超时后5秒重试，成功后统一按请求开始时间加20秒续期，不消费返回租期；地址为空时不发送发布请求，也不安排续期，已有记录按租期自然过期。mDNS 从实际 Internal 绑定、Dock socket 和网卡元数据建立实例，同网卡/IP 的端口合并；没有监听身份也维护查询资源。

运行期间 Server 集合固定。退出先排空当前发布批，停止后续续期，DDNS 记录及远端缓存自然过期，不发送撤回请求。随后关闭应用、shutdown 自有 mDNS 资源；startup/Network.init/监听登记失败也进入同一路径。Server.close 在等待前 take publisher，避免等待超时保留资源。没有新的取消 token、身份状态表、后台发布管理器或传输关闭接口。

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

Server 直接持有 Workspace 和 Chat 的 Arc；资源、数据库及路由清单见 [Workspace/Chat 接入清单](workspace-chat-interfaces.md)。这次保留目标分支现有业务模型和 worker 资源，每个模块使用自身已有的句柄与关闭信号；不额外添加 Server 投递任务集合。资源和队列在运行期间保持，close 清空 Router 并停止、等待两个模块的 worker。

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

每个 Server 直接持有一个 Sandbox，集中管理该身份的 Lib 集合、共享 WasmRuntime 引用与任务跟踪器。`new` 接收 `run` 创建并跨身份共享的 WasmRuntime，创建空 Lib 集合和空 TaskTracker。启动时串行加载 Lib，不需要内部锁。Lib 执行不限制并发数，不设置执行槽或 permit。静态和代理不登记到该 TaskTracker。

`load_libs` 每次扫描都重新校验并串行编译全部 Lib，构建局部候选；任一 Lib 加载失败直接返回错误，不修改当前 Lib 集合。完整扫描成功后更新自身集合。局部候选只是调用栈中的标准 HashMap，不引入构建会话、候选容器或发布状态。

`api_router` 根据 Sandbox 当前 Lib 集合构造 `/api` 分支，负责 Lib 查找、声明路径与方法验证、请求 URI 处理和 Invocation 执行。它只克隆本版本需要的 Lib 与共享执行资源；完整对外路径上的 daccess 授权由 Server 组装 Router 时统一添加，Server 保留完整 Router 的发布权。

`close` 同步关闭 TaskTracker 并清空集合，不等待任务。`wait` 在固定15秒内等待已关闭的 TaskTracker，超时返回 ShutdownDeadline。调用方 Server 清空 Router 并调用 Sandbox.close，再调用 `wait` 回收 WASM 任务。Sandbox 不持有自己的取消信号、身份、Endpoint、策略或派生计数；它不是独立的操作系统进程或容器。

2026-09-26 用户明确确认将 WASM 职责集中到 Sandbox：Server 的 `libs`、`runtime` 迁入已有 Sandbox，`sandbox` 改为直接所有；组件加载、替换和 API Router 方法归 Sandbox，构造与关闭签名相应调整。当时 `build_router` 接收标准 axum::Router，不再接收 Lib 集合或 Sandbox；该函数后来由 Server 中的显式 Router 组装取代。共享 WasmRuntime 由运行入口持有，Server 保留 Endpoint、授权、整体 Router；当次迁移保持 Invocation 和 StoreData 的字段及调用签名；随后取消 Lib 并发限制的变更见下文。

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

Runtime直接编译，启动时串行调用；不另存compile_slots、compile_tasks、cancel。Engine启用component model和consume_fuel，禁guest threads。linker注册WASI HTTP与既有identity WIT，instantiate_pre验证imports；不执行guest读取manifest。

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

## 9. 启停与错误

启动顺序：安装解析源并订阅地址事件 → 初始化一次全局 Network → 串行校验身份凭据、加载数据库配置/AccessService/组件 → 为每个需监听的 Server 启动 listen 任务。凭据读取、证书/私钥、OCSP 及发布用途验证失败记录身份名和原因并跳过，继续启动后续身份；跳过身份不保留 Server，下次启动重新尝试。配置、数据库及 Lib 错误仍结束启动；静态身份没有代理行也可启动。运行期间不更新 OCSP、不读取凭据或重载身份/配置/Lib。

Server.close 同步调用 Sandbox.close() 并清除 Router，随后等待 Sandbox 任务回收。`run` 不收回 listener 任务；运行期间不删除身份；身份目录变化在重启后生效。Server 不批量取消 HTTP 或审批；已进入的 HTTP 请求由其自身生命周期继续处理。`run` 退出时逐个调用 Server.close。全局 Network 不提供 shutdown；其连接池和后台维护随进程退出结束。`run` 直接等待进程退出信号，不另存取消 token。

每个 Server.close 的等待使用固定15秒上限；`run` 不设置全体 Server 的退出期限。超时明确返回ShutdownDeadline，不谎称任务已join。编译同步执行不可由Tokio abort中断，启动编译期间停机可能等当前编译返回；不为解决这一点暗加后台编译框架。

```rust
pub enum Error {
    BadRequest(String), InvalidConfig(String),
    InvalidComponent(String), InvalidIdentity(String), IdentityMismatch, MissingHandshake,
    RouteNotFound, MethodNotAllowed, Denied, Deadline,
    GuestExitedWithoutResponse,
    GuestRejectedResponse(wasmtime_wasi_http::p2::bindings::http::types::ErrorCode),
    Guest(wasmtime::Error), Io(std::io::Error), Database(sea_orm::DbErr),
    ConfigDatabase(rusqlite::Error), Http(http::Error), Dhttp(dhttp::Error),
    Task(tokio::task::JoinError), ShutdownDeadline,
}
impl Error { fn status(&self) -> http::StatusCode; fn body_error(self) -> dhttp::BoxError; }
```

Error实现Display/Error。业务拒绝在headers前生成HTTP响应；headers后只返回body error。Server/WasmRuntime/Invocation不同时保存正常资源和独立error标记；整体可用/失败结果可以直接用Result表达。

## 10. 验收边界

- 从Endpoint进入标准Service；Pishoo生产代码和测试驱动不传QPACK/H3写流。
- 启动串行加载 Lib 并构造 Router；运行期间不重载，配置/Lib/凭据变更在重启后生效。
- 按固定 daccess 分支验证允许、拒绝、202 持久审批、查询归属和一次性重试；验证联系人申请编号、轮询及幂等。Workspace/Chat 的本地管理要求 owner，公开资料仅放行精确 GET 端点。
- WASM提前响应继续上传、多值trailers、body替换、HEAD/204/304、超过4次并发执行及任务回收；Body丢弃不单独取消guest。
- 配置反代仅连接回环 HTTP/TCP 服务；同名身份的固定前缀 DHTTP 正向代理使用现有 Endpoint；Lib 的 WASI HTTP 出站一律拒绝。验证本机代理响应分块在上传 EOF 前到达，上传保持打开且模拟空闲31秒后仍可双向传输。
- 同名 Endpoint 共享本端身份连接池；Server.close 在退出时清空应用 Router，但不主动关闭共享传输。

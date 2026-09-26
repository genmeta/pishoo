# Pishoo 第一版结构与接口清单

本清单遵循[设计准则和接口约束](README.md)。范围是 HTTP 网关、WASM、既有 daccess 集成及[终端接缝](terminal-interfaces.md)。这是设计，不表示实现已完成。

## 1. 第一版边界

- Pishoo 只使用 dhttp Endpoint、标准 HTTP/Body、Tower/Axum；不持 QPACK、H3 writer 或 QUIC connection。
- Endpoint 独立 load；同名 Endpoint 共享通信状态。close 立即关闭该名称，终态保持到进程退出；临时停止接入使用 stop_listening。
- Server 串行加载和重载。没有 ServerState、Release、revision、构建队列或后台发布任务。
- Runtime 只保存 Engine/Linker。编译按当前调用顺序执行，不建立编译任务注册表或并发槽。
- 反代和 Lib 只允许通过当前身份的 dhttp Endpoint 出站；没有其他传输分支或普通 HTTP 客户端。
- 一个 `.wasm` component 文件就是一个 Lib，不另设 App 概念。接收和响应复用下述标准 Body 别名。局部流转换不是新的模块接口。

```rust
type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;
```

除 DaemonConfig、TerminalPolicy、run、validate_lib、Error 外，下文自有类型和模块接口均为 crate 内部；成员只在本crate所需模块间可见，不作为仓外API。字段列表完整，不增加通用状态map或预留成员。错误沿调用结果、Body或现成任务结果通道传播；互斥结果使用enum/Result，不在有效资源旁重复保存错误标记。

## 2. 配置和固定默认值

```rust
pub struct DaemonConfig {
    pub state_dir: std::path::PathBuf,
    pub terminal: TerminalPolicy,
}
// TerminalPolicy的唯一两个公开配置字段，完整定义归终端清单。
pub struct TerminalPolicy {
    pub enabled: bool,
    pub administrators: std::collections::HashSet<std::sync::Arc<str>>,
}
struct ServerConfig {
    listen: u8,
    proxy_locations: Vec<ProxyLocation>,
}
struct ProxyLocation {
    location: String,
    proxy_pass: http::uri::Parts,
}
```

DaemonConfig 从实例 `pishoo.toml` 读取，拒绝未知字段；state_dir 与启动时 DHTTP_HOME 指向同一实例目录，不在运行中修改进程环境。终端默认 disabled、管理员空集合。身份仍通过 dhttp-home 发现，Endpoint 使用相同身份目录。

config.db 保持 schema v1：settings(listen) 与 proxy_locations(location,proxy_pass)。settings 恰好一行；listen=0/1/2/3表示关闭/内网/外网/两者。proxy_pass 必须按 dhttp 目标规则校验；没有传输类型字段或数据库列。没有 lib_policies、policy_imports、默认策略来源账本或自动 schema v2 迁移。

2026-09-26 实施确认：用户批准将 `ProxyLocation.proxy_pass` 从 `http::Uri` 改为 `http::uri::Parts`。先校验完整 URI，再以 `path_and_query: None` 保留原始配置未写路径的事实；显式 `/` 则保存 `Some`。标准 `Uri` 会将两者规范化为相同值，无法落实既定的保留路径/替换前缀规则。不新增字段或自有结构。

这些运行限制是实现常量，不是配置字段：WASM 每次执行期限30秒、4个执行槽、总预留内存256MiB、在途fuel总额400_000_000；单次WASM内存64MiB、fuel100_000_000、每10_000 fuel让出；Store最多32 instances、32 memories、64 tables、100_000 table elements；每次最多16个出站；WASI输出1块、每块16KiB；签名输入1MiB、签名8KiB。扫描间隔2秒，本层退出等待上限15秒。终端使用终端清单自己的固定限制。

全局Network的监听配置由已验证Server的listen并集生成：`NetworkConfig { listen: vec![ListenConfig::Scope(scopes)] }`；没有额外配置字段。运行中新增身份所需范围超出启动并集时报告需重启。listen变化同样重启生效；Lib和路由重载不修改全局Network。

## 3. Daemon 和 Server

```rust
struct Daemon {
    home: dhttp_home::DhttpHome,
    servers: std::collections::BTreeMap<String, Server>,
    listeners: tokio::task::JoinSet<(String, Result<()>)>,
    runtime: std::sync::Arc<Runtime>,
    terminal: std::sync::Arc<TerminalManager>,
}
struct Server {
    profile: dhttp_home::identity::IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    access: std::sync::Arc<access_control::AccessService>,
    router: std::sync::Arc<std::sync::RwLock<axum::Router>>,
    libs: std::collections::BTreeMap<String, std::sync::Arc<Lib>>,
    runtime: std::sync::Arc<Runtime>,
    lib_slots: std::sync::Arc<tokio::sync::Semaphore>,
    terminal: std::sync::Arc<TerminalManager>,
    tasks: tokio_util::task::TaskTracker,
    cancel: tokio_util::sync::CancellationToken,
}
```

```rust
pub async fn run(config: DaemonConfig) -> Result<()>;
pub fn validate_lib(bytes: &[u8]) -> Result<oas3::OpenApiV3Spec>;
fn load_config(path: &std::path::Path) -> Result<DaemonConfig>;
fn load_server_config(profile: &dhttp_home::identity::IdentityProfile) -> Result<ServerConfig>;
fn network_config(servers: &[ServerConfig]) -> Result<Option<dhttp::NetworkConfig>>;
impl Daemon {
    async fn load(config: DaemonConfig) -> Result<Self>;
    async fn run(&mut self) -> Result<()>;
    async fn reload(&mut self) -> Result<()>;
    async fn shutdown(&mut self) -> Result<()>;
}
impl Server {
    async fn load(profile: dhttp_home::identity::IdentityProfile,
        runtime: std::sync::Arc<Runtime>, terminal: std::sync::Arc<TerminalManager>) -> Result<Self>;
    fn name(&self) -> &str;
    async fn reload(&mut self) -> Result<()>;
    fn listen(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output=Result<()>> + Send + 'static>>;
    async fn close(&mut self) -> Result<()>;
}
fn build_router(endpoint: dhttp::Endpoint, access: std::sync::Arc<access_control::AccessService>,
    libs: &std::collections::BTreeMap<String, std::sync::Arc<Lib>>,
    config: &ServerConfig, profile: &dhttp_home::identity::IdentityProfile,
    lib_slots: std::sync::Arc<tokio::sync::Semaphore>, tasks: tokio_util::task::TaskTracker) -> Result<axum::Router>;
```

Daemon单一循环串行处理扫描、Server加载/重载和删除。Server.listen在返回future前克隆Endpoint、router、信号和所需共享对象，不借用Server；因此监听运行期间actor仍可 `reload(&mut self)`。

每次请求仅短暂read-lock并clone当前Router，然后释放锁再驱动oneshot。handler捕获本路由的Arc<Lib>和必要对象，不捕获Server。旧请求保有旧Router/Lib，不需要另一个发布对象。

reload在局部变量中读取候选配置/组件、逐个串行编译和构造Router。全部Router装配成功后取得write-lock，一次replace；随后更新Server.config/libs。单Lib坏候选保留原Arc；整体Router失败保留旧Router。没有部分挂入路由的中间状态。

组件一次读出的bytes同时用于OpenAPI、摘要和编译。重载提交前重查身份目录仍存在、输入文件摘要仍匹配；输入已变则丢弃本次候选，下轮重读。没有后台编译结果，也没有跨任务revision检查。删除与替换只由该actor执行。

新版本复用该Lib现有取消token，新Lib保存本版本目录权限；删除Lib取消这个共享token并移除路由。旧请求仍持有的旧版本会收到相同取消。重新出现的Lib建立新token。删除Server调用close；同名Endpoint已关闭，本进程不自动复活该Server。

静态文件每请求固定已打开句柄。部署用临时文件+原子rename，不原地改写正在读取的文件；此为部署约束，不宣称整个file目录是不可变发布快照。

## 4. 以当前 daccess 库为准

依赖是同进程的 `access_control` crate。接口以当前 daccess 库为准，已核对核心接口所在提交为 `origin/main@1ec62d4`；`pishoo/feat/daccess@b22dac3` 只作为可复用的集成参考。不兼容处按当前库调整 Pishoo，不要求补回旧 API。

每Server加载一次profile的db/access.db；创建父目录，使用 `sqlite://<path>?mode=rwc`。owner name取有效本端身份全名，SubjectId取DHTTP证书SKI的owner_hash文本bytes，然后调用既有 `AccessService::load_from_db(uri,name,&subject_id)`。

```rust
async fn authorize(
    axum::extract::State(access): axum::extract::State<std::sync::Arc<access_control::AccessService>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response;
fn management_router(access: std::sync::Arc<access_control::AccessService>,
    profile: &str, owner_name: &str) -> axum::Router;
```

可信身份使用 request extensions 中既有 HandshakeSummary，核对 local 与 Endpoint；remote.name 及 certificates 构造库已有的 Visitor。summary 缺失是接入错误，存在且 remote=None 才是匿名；无效 SKI 拒绝。清除旧 Visitor 后注入新 Visitor；不用 ClientNameResolver、连接缓存或新身份 Context。

调用库现有 `access.auth(headers, name, subject_id).await`。已验证身份同时传 name 和 SubjectId；匿名同时传 None。Headers 填写 method、完整 path_and_query、headers 和 `request_id: None`。默认 owner allow、其余 deny 及已存规则的加载交给 AccessService，不在 Pishoo 重复实现。

| 库的返回值 | 当前请求的处理 |
| --- | --- |
| `AuthResult::Allowed` | 调用业务 handler |
| `AuthResult::Denied` | 返回 403 |
| `AuthResult::Reviewing(id, state, registry)` | 在本次 authorize 中等待 `state.await`；`Ok(Action::Allow)` 才进入 handler，`Ok(Action::Deny)` 或 `Err(RequestResetError)` 返回 403 |
| `Err(DbErr)` | 日志记录细节，对外返回 500 固定消息 |

进入 Reviewing 分支后立即用现成 `scopeguard` 登记局部清理：调用 `state.cancel()` 和 `registry.del(id)`。审批结果返回、错误或 authorize future 被丢弃时均执行；在调用业务 handler 前完成清理。这里只删除 live 登记，不删除数据库中的持久记录。审批等待直接属于本次请求，不 spawn 等待任务，不新增 PendingReview、Guard、信号或布尔状态。主动取消复用请求自身的生命周期。

当前库同时支持实时审批和带 RequestId 的持久审批；第一版 Pishoo 只使用 `request_id: None`，不定义 RequestId 的 HTTP 传输方式，也不替请求自动生成 ID。数据库中的既有持久记录继续由库的管理 API 处理。旧分支的 202 响应、status_url、状态路径识别函数及 `/contact/self` 专用协议不纳入本版接口。

管理路由直接调用 `access_control::management_router(access)` 并 merge 在根，复用库当前的 `/contact`、`/contacts`、`/contact/{name}`、`/acl/*`；审批管理为 `GET /acl/reviews/live`、`GET /acl/reviews/persistent` 和 `PATCH /acl/review`。具体请求体、响应体与行归属校验交给库。所有管理路由同样通过 authorize，不保留旧状态查询的 ACL 豁免。Pishoo 不新增 notifier、出站审批连接器、重试队列或自动默认规则导入。

此挂载未提供 ContactNotifier；需要远端通知的联系人 Syncing 操作按库返回 503，不能显示为同步完成。本版不承诺完成这条跨端同步流程。

Workspace 保留 `/workspace`、`/workspace/`、`/workspace/{*path}`；`/workspace-api/context` 返回已有 profile、owner_name、development_identity=false、demo_data=false、version 字段。此处 management_router 负责把这些 Pishoo 路由与库的管理路由组合；旧分支对应函数名为 management_app。管理前端按当前库 API 适配，不要求旧前端未经修改即可使用。

Workspace、管理 API、静态/代理/Lib 入口统一套 authorize。Lib 在剥离 `/api/<LibId>` 前按完整对外路径授权。`/workspace` 返回 307 到 `/workspace/`；无扩展名深链回 index.html，缺失 asset 返回 404；没有 `/admin` 兼容路径。

## 5. Lib 执行准入与隔离

一个 Server 的所有 Lib 共用现成 `Arc<Semaphore>`，固定4个执行槽，不再定义 Sandbox 包装及其 new/admit/active 方法。这个信号量只限制 Lib 执行，静态、代理和终端不占它的名额。

请求先匹配 Lib API 并通过 daccess 授权，再通过 `try_acquire_owned()` 取得现成 OwnedSemaphorePermit；满额返回429。permit 传入 Invocation，随后由实际 Store 持有，Store 回收即归还。重载复用同一份 lib_slots，旧版本执行仍占原来的槽。Server 或 Lib 已取消时不得从旧 Router 启动新执行。

这只负责资源准入，不代表隔离。隔离由每次执行的独立 Store/Instance、受限 WasiCtx、Lib 私有 `/data`、实际内存 limiter、fuel 和宿主能力控制完成。每次内存上限64MiB、fuel100_000_000，4个槽对应原有256MiB内存及400_000_000 fuel预留，不另存派生计数或租约结构，也不等待传输FIN/ACK。

Pishoo流缓冲使用固定大小，outgoing总次数按invocation限制；不引入另一套通用BufferLease账本。Store内存限额不代表Wasmtime全部宿主分配或磁盘已受总字节硬配额，不作这一保证。

## 6. Runtime、Lib 和 Store

```rust
struct Runtime {
    engine: wasmtime::Engine,
    linker: wasmtime::component::Linker<StoreData>,
}
struct Lib {
    id: String,
    digest: [u8; 32],
    openapi: oas3::OpenApiV3Spec,
    component: wasmtime::component::Component,
    runtime: std::sync::Arc<Runtime>,
    filesystem: wasmtime_wasi::filesystem::WasiFilesystemCtx,
    policy: LibPolicy,
    cancel: tokio_util::sync::CancellationToken,
}
struct LibPolicy { data_write: bool, outgoing: Vec<OutgoingRule>, sign: bool, verify: bool }
struct OutgoingRule { methods: Vec<http::Method>, origin: http::Uri, path_prefix: String }
struct StoreData {
    table: wasmtime::component::ResourceTable,
    wasi: wasmtime_wasi::WasiCtx,
    http: wasmtime_wasi_http::p2::WasiHttpCtx,
    memory: MemoryLimits,
    outgoing: HostOutgoing,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
    policy: LibPolicy,
    permit: tokio::sync::OwnedSemaphorePermit,
}
struct MemoryLimits { base: wasmtime::StoreLimits, used: usize, pending: usize }
```

```rust
impl Runtime {
    fn new() -> Result<Self>;
    fn compile(&self, bytes: &[u8]) -> Result<wasmtime::component::Component>;
}
impl Lib {
    fn load(runtime: std::sync::Arc<Runtime>, id: String, bytes: &[u8], data_dir: &std::path::Path,
        policy: LibPolicy, cancel: tokio_util::sync::CancellationToken) -> Result<Self>;
}
impl Default for LibPolicy { fn default() -> Self; }
```

Runtime直接编译，启动/重载串行调用；不另存compile_slots、compile_tasks、cancel。Engine启用component model和consume_fuel，禁guest threads。linker注册WASI HTTP与既有identity WIT，instantiate_pre验证imports；不执行guest读取manifest。

每个Lib只预打开自己的lib/<LibId>/data为/data，按本版本policy设置读写，再保存filesystem clone；禁止身份根、db、ssl和兄弟Lib。使用现有WasiCtxBuilder.preopened_dir，不发明cap-dir注入API。新版本重新构造grants，不能复用旧写权限。

LibPolicy是内部宿主能力值，不是第二套配置。v1部署默认data_write=true，outgoing空，sign=false，verify=false；测试/宿主明确传入授权值才可开放对应能力。guest声明本身不授予权限。不增加逐Lib策略表、上限交集或权限热更新框架。

Runtime/Lib/StoreData/Invocation/HostOutgoing同属Lib执行模块，构造用完整字段，出站复用当前身份的Endpoint。StoreData实现现成WasiView、WasiHttpView及identity WIT host；MemoryLimits实现ResourceLimiter，保留实际多memory求和used与失败回滚pending，直接比较固定64MiB，不再保存一份ceiling。Store在instantiate前安装limiter、fuel和fuel_async_yield_interval；deadline/取消由独立supervisor观察并终止、await guest，不能仅靠Body下一次poll发现超时。

## 7. Lib 执行与响应体

```rust
struct Invocation {
    lib: std::sync::Arc<Lib>,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
    endpoint: dhttp::Endpoint,
    producer_cancel: tokio_util::sync::CancellationToken,
    permit: tokio::sync::OwnedSemaphorePermit,
    tasks: tokio_util::task::TaskTracker,
}
enum LibResponseBody {
    Reading {
        inner: wasmtime_wasi_http::p2::body::HyperOutgoingBody,
        guest: Option<tokio::task::JoinHandle<Result<()>>>,
        cancel_on_drop: tokio_util::sync::DropGuard,
    },
    Waiting {
        guest: tokio::task::JoinHandle<Result<()>>,
        trailers: Option<http::HeaderMap>,
        cancel_on_drop: tokio_util::sync::DropGuard,
    },
    Ended,
}
impl Invocation {
    fn new(lib: std::sync::Arc<Lib>, permit: tokio::sync::OwnedSemaphorePermit,
        endpoint: dhttp::Endpoint,
        handshake: &dhttp::HandshakeSummary,
        tasks: tokio_util::task::TaskTracker) -> Result<Self>;
    async fn execute(self, request: http::Request<Body>) -> Result<http::Response<Body>>;
}
```

LibResponseBody 实现标准 http_body::Body，直接用 enum 分支持有有效资源。析构取消复用 Tokio 已有 DropGuard，不再实现自定义 guard 或额外 Drop 状态。Reading 才有可读 WASI body；读完后若 guest 尚未返回，转入 Waiting 并只保留待取的任务结果与最终 trailers；结束或错误后转入 Ended，不再保存正常流或取消句柄。Reading 内的 guest=Some/None 只表示独立任务是否仍需读取结果，不能用它代表 body 是否结束。

Lib 首次加载时从 Server.cancel 派生取消 token，重载版本复用该 Lib token；每次 Invocation 从 Lib token 派生 producer_cancel。因此 Server 关闭、Lib 删除、单次 producer 被放弃分别作用于各自范围，无需额外应用租约。

supervisor 在执行开始时用局部变量计算固定30秒 deadline，从实例化起覆盖 guest 和其出站工作。到期直接取消 producer、终止 guest 并回收子任务，不新增 watchdog、deadline 成员或通知接口。Store 实际回收后才归还 Lib 执行 permit，不能因发出取消就提前归还；出站子任务继续由既有 children 跟踪并收尾。

调用开始取得现成 `producer_cancel.clone().drop_guard()`，随后登记并启动 supervisor；收到 outparam 后将这个既有取消所有权直接移动进 LibResponseBody::Reading，构造完整响应后返回。Reading/Waiting 转换移动同一个所有权，不新增布尔标记；正常完成时 disarm 再转为 Ended，错误或提前丢弃则由既有工具发出主动取消。

supervisor 持 guest 执行任务，等待其返回或响应主动取消/期限；取消后终止并 await guest，随后回收子请求，再直接返回 Result<()>。TaskTracker.spawn 的 JoinHandle 交给 LibResponseBody，去掉另建的完成 oneshot。响应提交与任务退出同时就绪时检查 outparam：已确认成功的任务无需再保存，未完成任务保留 Some(handle)，失败在交付响应头前直接返回。不能用 is_finished 代替读取结果，也不能重复 await。

LibResponseBody 先流式交付 data；WASI body 结束后读取仍持有的任务返回值，处理 JoinError 并取走 handle，再交付暂存 trailers/正常 EOF；失败交付一次 body error。正常EOF后Drop不取消。提前Drop只取消本producer的child token，不能取消最终替换响应或关闭连接。

Wasmtime 47 的 trailers 是最终帧：有待取任务结果时直接转 Waiting 并保留 trailers；任务已成功结束时 disarm、转 Ended 并返回 trailers，下一次 poll 为 EOF。无需再加 trailers_seen 或 completed 标志。

静态和代理直接使用现成 Body；文件、上传源和上游响应由各自的流对象持有，提前 Drop 沿既有适配停止对应 I/O。上传自然 EOF 不取消响应方向。HEAD/204/304、middleware 替换 body 时回收旧 producer，最终合法响应由 dhttp 继续发送；不需要通用租约 Body。

## 8. 反代、Lib 出站与身份签名

```rust
struct HostOutgoing {
    endpoint: Option<dhttp::Endpoint>,
    policy: LibPolicy,
    remaining_requests: usize,
    children: tokio_util::task::TaskTracker,
    cancel: tokio_util::sync::CancellationToken,
}
async fn proxy(endpoint: dhttp::Endpoint, route: ProxyLocation,
    request: http::Request<Body>) -> Result<http::Response<Body>>;
```

HostOutgoing.cancel 从本次 producer_cancel 派生，取消执行会向下传播；guest 结束时可单独取消剩余出站工作，不反向取消已经决定的响应。

HostOutgoing实现真实WasiHttpHooks的send_request、outgoing_body_buffer_chunks、outgoing_body_chunk_size，后两者固定1和16KiB。endpoint是获准的源身份能力：可信调用者为Server自身时为Some，其他情况为None，不再重复保存caller_is_self。WASI专属类型留在此适配；获准的请求直接调用endpoint.from_request(request).await。guest不能选择源身份。未获授权或没有Endpoint时直接拒绝，不调用Wasmtime默认网络发送器。

仅持有源Endpoint能力、命中内部宿主目标/method规则且剩余次数足够时允许Lib出站；ACL/审批/联系人间接授权管理路径始终禁止。实际目标每次检查，禁止自动重定向/重试。清除保留身份头。remaining_requests仅实现固定16次上限，children只用于确认本invocation子请求真正回收，不是另一个全局运行时。child task持有本次出站流和取消能力、登记children；guest结束后取消剩余child并wait，supervisor以函数返回值交付结局。

proxy 完成路径/query、authority 和转发头处理后，直接调用当前 Server 的 `endpoint.from_request(request).await`。HostOutgoing 完成 Lib 出站授权后调用同一接口。两处按 dhttp 的规则校验实际目标，不提供普通 HTTP/HTTPS 上游模式，不按连接失败切换传输；错误直接返回。保留多值 headers/trailers、流式背压和取消。没有 UpstreamKind 或只做协议分派的 send_upstream 包装。

已有 `pishoo:identity/signatures@0.1.0` 的sign/verify保留。StoreData检查能力、固定输入上限和现有HostOutgoing.cancel；异步身份解析直接在该取消指令与操作返回之间select，不新增取消成员；sign调用dhttp::certificate::sign(&local,data)，由helper选择规范SignatureScheme。verify优先用已验证local/remote公钥，否则在出站准入后resolve_remote；使用verify_signature，不能调用不存在的RemoteAuthority.verify。缺权限映射既有WIT错误，不把私钥/authority交guest；不承诺离线历史证书查询。

## 9. 启停、终端与错误

启动顺序：读实例和profile配置 → 串行加载有效身份/AccessService/组件 → 初始化一次全局Network → 每Server启动受跟踪的listen。单坏Lib保留或跳过，单坏身份不影响其他身份；静态身份没有代理行也可启动。

入口先验证Server身份；终端保留路径在普通pishoo-*清洗之前交TerminalManager，使其读取协议版本头。终端只按实例管理员名单授权，不因访问者等于Server而自动放行；其结构和固定平台行为见[终端清单](terminal-interfaces.md)。

Server.close立即取消自己的token、同步调用Endpoint.close()，然后等待本方tasks；Daemon收回listener，不先排空新子请求。临停同步调用stop_listening，可在同名未close时重新监听；已经close的名称在本进程不能重新打开。Daemon退出先关闭所有Server、回收本方任务/终端，最后对已初始化的全局Network同步调用shutdown()；Network不接deadline、不返回额外关闭报告。Daemon.run直接等待进程退出信号，不另存daemon cancellation token。

Pishoo自身等待使用固定15秒上限，终端清理使用其约定；超时明确返回ShutdownDeadline，不谎称任务已join。编译同步执行不可由Tokio abort中断，v1串行重载期间停机可能等当前编译返回；不为解决这一点暗加后台编译框架。

```rust
pub enum Error {
    BadRequest(String), BackendUnavailable(String), InvalidConfig(String),
    InvalidComponent(String), InvalidIdentity(String), IdentityMismatch, MissingHandshake,
    RouteNotFound, MethodNotAllowed, Denied, Capacity, Cancelled, Deadline, Closed,
    GuestExitedWithoutResponse,
    GuestRejectedResponse(wasmtime_wasi_http::p2::bindings::http::types::ErrorCode),
    Guest(wasmtime::Error), Io(std::io::Error), Database(sea_orm::DbErr),
    ConfigDatabase(rusqlite::Error), Http(http::Error), Dhttp(dhttp::Error),
    Task(tokio::task::JoinError), ShutdownDeadline,
}
impl Error { fn status(&self) -> http::StatusCode; fn body_error(self) -> dhttp::BoxError; }
```

Error实现Display/Error。业务拒绝在headers前生成HTTP响应；headers后只返回body error。Daemon/Server/Runtime/Invocation不同时保存正常资源和独立error标记；整体可用/失败结果可以直接用Result表达。terminal建立后错误沿其ERROR/EXIT协议或body error返回。

## 10. 验收边界

- 从Endpoint进入标准Service；Pishoo生产代码和测试驱动不传QPACK/H3写流。
- 串行reload一次换Router；失败保留旧Router；旧请求持旧Lib，删除Lib取消其所有在途版本。
- 按当前 daccess 库验证允许、拒绝、审批批准/否决/取消及 live 登记清理；管理 API 在根路径挂载并经过授权，管理界面与当前库一致。旧分支兼容不作为阻塞条件。
- WASM提前响应继续上传、多值trailers、CPU取消、body替换、HEAD/204/304和Lib执行槽归还。
- 反代与Lib仅经当前身份的Endpoint出站；目标不符合dhttp规则或连接失败时直接报错，不启用其他传输。
- 同名Endpoint共享；close立即终止且不重开，stop_listening仅临停；单Server关闭不误关其他名称。
- 终端按终端清单验证；设计文档不替代平台隔离验收。

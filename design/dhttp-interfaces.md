# dhttp 第一版结构与接口清单

本轮只更新设计，不实施代码；遵循[清单约束](README.md)。h3x 的既定接口、结构和协议行为保持不变。DHTTP 只封装 Endpoint、共享网络、应用接入与流适配，不增加传输配额、交换控制、完成订阅或错误缓存。

## 1. 边界与现成类型

- Endpoint 只持规范化名称；独立 load 不访问 Network。
- 同规范化名称代表同一逻辑 Endpoint。所有同名句柄共享关闭效果，不区分 load 次数。
- Network 在进程内初始化一次；负责实际连接、监听登记和后台任务。
- close 是立即关闭：禁止新操作、发出取消、关闭已建立连接，不等待排空或回收报告。
- close 后该名称在本进程中保持关闭，必须重启进程才可恢复。临时停服使用 stop_listening，随后可以重新 listen。
- Pishoo 反代和 Lib 的出站只通过当前身份的 Endpoint；不增加其他 HTTP 传输或失败降级路径。
- Pishoo 只等待自己的应用任务；DHTTP 不提供 finished、ExchangeControl、RequestInfo 或 Peer。

直接复用 http/http-body、http-body-util、Tower、Tokio、tokio-util、async-stream 和 scopeguard。以下签名省略这些现成类型的 use 声明。

```rust
pub use qtls::{HandshakeSummary, LocalAuthority, RemoteAuthority, CertificateDer};
pub use qconn::{Scope, Scopes};
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type Result<T> = std::result::Result<T, Error>;
pub type EmptyBody = http_body_util::Empty<Bytes>;
pub type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, BoxError>;
pub type RequestFuture = Pin<Box<
    dyn Future<Output = Result<http::Response<Body>>> + Send + 'static,
>>;
type ErasedService = tower::util::BoxCloneService<
    http::Request<Body>, http::Response<Body>, BoxError,
>;
type ConnectionKey = (Arc<str>, Arc<str>); // 本端规范化名称、远端规范化名称
type H3 = h3x::H3Connection<QuicTransport>;
static NETWORK: tokio::sync::OnceCell<DhttpNetwork> = tokio::sync::OnceCell::const_new();
```

Error 复用现有错误类型；失败通过 Result、流错误或任务返回传播。需要表达互斥的可用/失败结果时，使用 Result<可用对象, Error>，不在正常对象旁另存 error、failed、closed 标记。不引入独立 Response future 结构或 ShutdownReport。

## 2. Endpoint 与可 await 请求

```rust
#[derive(Clone)]
pub struct Endpoint { name: Arc<str> }
impl Endpoint {
    pub async fn load(name: impl AsRef<str>) -> Result<Self>;
    pub fn name(&self) -> &str;
    pub fn get(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn head(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn post(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn put(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn patch(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn delete(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn options(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn request(&self, method: http::Method, uri: http::Uri) -> Request<EmptyBody>;
    pub fn from_request<B>(&self, request: http::Request<B>) -> Request<B>;
    pub async fn listen<S, B>(&self, scopes: Scopes, service: S) -> Result<()>
    where
        S: tower_service::Service<http::Request<Body>, Response = http::Response<B>>
            + Clone + Send + 'static,
        S::Future: Send + 'static,
        S::Error: Into<BoxError>,
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>;
    pub fn stop_listening(&self) -> Result<()>;
    pub fn close(&self) -> Result<()>;
}

pub struct Request<B> {
    endpoint: Endpoint,
    message: http::Request<B>,
}
impl<B> Request<B> {
    pub fn header(self, name: http::HeaderName, value: http::HeaderValue) -> Self;
    pub fn append_header(self, name: http::HeaderName, value: http::HeaderValue) -> Self;
    pub fn body<T>(self, body: T) -> Request<T>;
}
impl<B> std::future::IntoFuture for Request<B>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    type Output = Result<http::Response<Body>>;
    type IntoFuture = RequestFuture;
    fn into_future(self) -> Self::IntoFuture;
}
```

URL 和 header 先解析为有效类型，构造请求时不保存待报错状态。所有方法默认空 body，`.body(B)` 可替换为任意标准流式 body；不按方法区分消息模型。首次轮询 await 才开始网络操作；header/body 构造不需要 Network。Request 不 Clone；发送后不再向调用方交付可编辑头字段的 builder。

```rust
let uri: http::Uri = url.parse()?;
let response = endpoint.get(uri)
    .header(http::header::ACCEPT, http::HeaderValue::from_static("application/json"))
    .await?;
```

上传与响应读取并发进行，返回响应头不等待完整上传。from_request 与 fluent 请求走相同内部驱动；没有额外公开 execute/send。

## 3. Network：配置与全部实际资源

```rust
#[derive(Clone)]
pub struct NetworkConfig { pub listen: Vec<ListenConfig> }
#[derive(Clone)]
pub enum ListenConfig {
    Scope(Scopes),
    Interface { device: String, scopes: Scopes },
}

pub struct DhttpNetwork {
    config: NetworkConfig,
    endpoints: Mutex<HashMap<Arc<str>, Arc<EndpointConnections>>>,
    listeners: Mutex<HashMap<Arc<str>, Arc<ListenerRegistration>>>,
    pool: h3x::Pool<ConnectionKey, QuicTransport, Error>,
    bindings: Mutex<HashMap<(String, IpAddr), Binding>>,
    addresses: qprotocol::AddressBook,
    stop: CancellationToken,
}
impl DhttpNetwork {
    pub async fn init(config: NetworkConfig) -> Result<&'static Self>;
    pub fn global() -> Result<&'static Self>;
    pub fn shutdown(&self) -> Result<()>;
}

struct EndpointConnections {
    stop: CancellationToken,
    connections: Mutex<Vec<H3>>,
}
struct Binding {
    socket: Arc<qprotocol::UdpSocket>,
    scopes: Scopes,
    device_index: u32,
}
```

EndpointConnections 只表示该身份持有的连接和取消信号，不是状态机；没有额外方法，只用结构字面量构造。stop.is_cancelled 就是关闭标记；connections 包含该名称的全部活动连接，包括匿名对端、未放入复用池的连接以及正在结束的连接。h3x::Pool 仅用于复用，不能代替完整活动连接集合。Vec 通过现有连接 Arc 的指针判断是否为同一连接，无连接 ID 或世代字段。

Network 不保存 TaskTracker，因为立即关闭不等待任务计数或生成回收报告。listen 自己等待其 supervisor 的 JoinHandle；读写 driver 持有实际连接/原生流，在各自退出分支释放资源。确需等待并发子任务时，由该操作的局部 JoinSet 负责，不建立全局任务账本。Network.stop 标识整体关闭；shutdown 后不能再次 init。已关闭名称的 EndpointConnections 留在表内，避免旧句柄重新打开。

Binding 没有额外方法或 Drop 机制，实际地址直接读取 socket.local_addr()，不另存副本。网络维护任务在同一次更新中显式撤销 AddressBook/协议/Dock 登记后释放 socket；shutdown 也走这条实际清理路径。设备和地址变化只重新应用原 listen 规则，配置更新需要重启。

超时和队列容量使用模块内部常量，不放进 NetworkConfig：CONNECT_TIMEOUT、OPERATION_TIMEOUT、BODY_WINDOW_BYTES、BODY_READ_CHUNK_BYTES、ACCEPT_QUEUE_CAPACITY。它们只规定单次操作或局部缓冲，不构成全局/逐 Endpoint 配额。普通请求与终端的业务期限仍由 Pishoo 决定。

## 4. 应用服务接入

```rust
struct ListenerRegistration {
    service: Mutex<ErasedService>,
    stop: CancellationToken,
    qconn_owner: Arc<qconn::Server>,
}
```

没有 ListenerPhase、ListenGuard、ServiceAdapter 或监听专用状态机。name 已是 Network 表键；scopes 使用 qconn::Server 的已有字段，不重复存储。

为了让同一服务表接收不同具体类型的 Router/handler，直接组合现成 Tower 工具：map_err 把 S::Error 转为 BoxError，map_response 把响应 body 装箱，boxed_clone 得到 ErasedService。每请求克隆实例后通过现成 oneshot 或 ready+call 驱动同一个实例，不把 readiness 和 call 分给不同克隆。

监听提交顺序：

1. 先异步读取并验证凭据，期间尚未占用监听名称；随后创建供本次监听使用的局部 oneshot 通道。
2. 取得该名称 EndpointConnections，按固定顺序锁 connections，再锁 listeners；检查 Network/Endpoint 未关闭且名称尚未监听。
3. 持锁调用同步 qconn.listen，取得实际 Arc<qconn::Server> 后构造完整 ListenerRegistration 并插入表，再启动本次 supervisor。锁本身保证同名登记互斥，不创建占位或空 OnceLock。
4. 提交临界区中没有 await。同步失败立即返回错误；已取得的底层登记在返回前撤销。不留下 Preparing 状态或半完成的登记。
5. listen future 局部持一个 oneshot Sender；supervisor 持 Receiver。Sender 的生存期明确跨越整个监听等待，future 被丢弃时通道关闭，supervisor 据此停止并撤登记。
6. 公共 listen 等待本次 supervisor 的 JoinHandle 返回 Result<()>；错误直接沿返回链交付，不缓存到登记字段。

取消按 Network.stop → EndpointConnections.stop → ListenerRegistration.stop 的 child_token 关系派生。supervisor 只观察本次监听 token 和局部 oneshot，不重复订阅祖先；这些用于主动停止，不是流终态通知。它的真实 qconn 撤销和任务退出负责取消路径收尾，不能把必须执行的清理只写在 public listen future 的正常返回路径。

stop_listening 按当前登记取消 stop，并在 listeners 锁内按 qconn_owner 的 Arc 指针撤销真实 qconn 登记、移除本次表项；不等待旧任务结束。supervisor 的重复清理必须比对当前表项/实际 Server Arc，不能删除后来重新 listen 的登记。

qconn 接收回调在同步登记前捕获当时的 EndpointConnections、本次监听 token 和名称。接受结果经固定有界交接队列交 supervisor；队列满或接收方消失时立即关闭该连接。接入任务在相同锁序下检查捕获的 token 未取消且名称仍有当前登记；stop_listening 先取消旧 token 再移除登记，所以旧回调不能接到新登记。迟到结果直接关闭。supervisor 清理时再比较其持有的实际 Server Arc。

## 5. 立即关闭的并发规则

close 取得该名称的 EndpointConnections；尚无活动状态时建立并取消它，保证后续同名请求也被拒绝。所有同名句柄操作相同表项。

connections 的锁同时承担该名称的准入同步：启动任务或登记连接时，在锁内检查 Network.stop 与 Endpoint.stop。close 在同一把锁内执行 Endpoint.stop.cancel() 并取走全部连接，然后撤销监听，锁外逐条关闭已建立连接。不得先在锁外检查 token，再把连接插入已经清空的集合。

锁顺序固定：先短暂取得 endpoints 表项并释放表锁；需要同时操作时依次锁 EndpointConnections.connections、Network.listeners。禁止反向加锁，锁内不 await、不调用应用 Service。

已登记连接关闭时，使用现有 pool.remove_connection(key, &connection) 精确撤销对应复用条目。尚在建连的请求持有当时的 EndpointConnections；关闭信号停止本层等待或后续接入，迟到的已建立结果必须关闭并移除。因为同名终态不重开，不需要 generation 或 OwnerKey。

close/shutdown 必须尝试处理全部已取得的连接和登记，不能因第一项失败就跳过其余资源；失败沿本次 Result 返回，不缓存。返回仅表示取消已发出、当前已建立连接和登记已处理，不表示所有任务、对端流或应用执行已经结束。driver 继续在本操作的所有权下回收自己的资源；不增加独立的结束或失败订阅。当前 qconn.connect 的底层任务是否能随等待 future 丢弃而立刻结束，取决于其既有取消契约；本清单不宣称本地尚未提供的能力已经实现，也不为此增加旁路状态结构。

stop_listening 不取消 EndpointConnections.stop。停止监听后可重新 listen，也仍能主动发请求；已接入交换可以继续。服务删除若调用 close，则本进程内不能同名重新接入；临时撤服务和组件更新必须使用 stop_listening/Router 更新。

## 6. 原生传输适配

```rust
#[derive(Clone)]
struct QuicTransport {
    connection: Arc<qconn::ArcConnection>,
    handshake: Arc<qtls::HandshakeSummary>,
    role: h3x::Role,
}
struct RecvStream(qtransport::StreamReader);
struct SendStream(qtransport::StreamWriter);
```

保留已有 Transport/AsyncRead/AsyncWrite/StopSending/CancelStream/TransportError 实现；不增加自定义关联方法、成员或 Drop 行为。这些既有 trait 的方法签名保持原样。QuicTransport 直接转发 native 开流、接流和 close；Recv/Send 直接转发 I/O 与取消，不增加广播、统计或完成状态。

DHTTP 对每条已建立连接只启动一次请求接入循环；入站和出站连接都由 Network 跟踪。连接接受请求时取得当前名称的监听 Service，没有有效监听则拒绝本次请求，不关闭仍用于其他请求的连接。

## 7. Body 适配没有补建协议能力

Body 只是标准 UnsyncBoxBody 别名，没有成员、方法、错误缓存或 consumed channel。h3x 原生消息已经负责 HTTP 消息、body、trailers、EOF、读写错误和 stop/cancel。DHTTP 的转换只为连接已有接口：

- 接收：在局部 StreamBody/async stream 中读原生数据，产生 Frame::data；数据 EOF 后交付已有 trailers。错误直接返回，之后不伪造 trailers。
- 提前放弃：进入生成器前建立现成 scopeguard，持有原生接收方向；正常 EOF 后解除 guard，提前 Drop 调用已有 stop。无需自定义 Guard 类型。
- 发送：标准 Body 的 DATA/trailers 写入原有 h3x 可写消息；正常 EOF 调用 shutdown，失败调用既有 cancel；与固定 h3x writer future 并发推进。
- HEAD/204/304 等消息语义沿用 h3x。DHTTP 处理被抑制的应用 producer，Pishoo 回收自己的应用任务。

适配所需的纯模块内函数、闭包和 async 局部变量属于方法实现，不列成另一套冻结公共函数。标准适配不会反向要求 h3x 增加新的消息类型或生命周期接口。

操作超时只作用于当前可观察的等待阶段，不声称观察到 h3x 未公开的 native 进度；不能给整个长流 writer future 套短总时长限制。终端适用足够长的底层操作期限，自己的建立/空闲/会话期限由终端模块落实。

## 8. 身份与签名接缝

```rust
pub fn subject_id(certificates: &[qtls::CertificateDer<'_>]) -> Result<Vec<u8>>;
pub fn sign(local: &qtls::LocalAuthority, data: &[u8]) -> Result<Vec<u8>>;
pub fn verify_signature(spki: &[u8], data: &[u8], signature: &[u8]) -> Result<bool>;
pub async fn resolve_remote(endpoint: &Endpoint, name: &str) -> Result<qtls::RemoteAuthority>;
```

这四个 certificate 函数是 Pishoo 的跨仓接缝，保留冻结。subject_id 沿现有 DHTTP SKI owner_hash 文本字节规范；sign/verify 复用既有规范算法；resolve_remote 取得实际握手验证的对端，不承诺离线或历史证书查询。凭据读取和信任装配继续复用现有 home/trust 内部代码，不新建身份结构。

成功入站把实际 HandshakeSummary 放入 request extensions；缺少摘要是接入错误，remote=None 才表示匿名。LocalAuthority 的签名能力留在可信宿主，guest 只经 Pishoo 授权的接口使用。出站忽略转带的可信身份 extensions，使用当前 Endpoint 的身份。

## 9. 固定调用关系

1. Endpoint.load → 名称规范化；get/post/request/header/body → 请求字段构造，不访问 Network。
2. Request.await → global Network → 检查名称状态 → Pool 取得连接 → open_bi → 并发 write_request/read_response。
3. Endpoint.listen → 准备凭据 → 原子登记 → supervisor → accept_bi → read_request → 标准 Service → write_response。
4. Service/代理/WASI 使用标准 Body；适配器只桥接数据、trailers 与原有流结束语义。
5. stop_listening → 撤当前监听；close → 取消同名全部网络操作并关闭连接；shutdown → 关闭所有名称及共享网络。

没有 OwnerKey、自定义 ConnectionKey 结构、Phase、NetworkState、ListenerPhase、ServiceAdapter、ListenGuard、ShutdownReport、ExchangeLease 或精细关闭计数。保留的自有结构只有 Endpoint、Request、NetworkConfig、ListenConfig、DhttpNetwork、EndpointConnections、ListenerRegistration、Binding、QuicTransport、RecvStream、SendStream；Error 沿用现有类型。

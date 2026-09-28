# dhttp 第一版结构与接口清单

本清单定义当前设计；遵循[清单约束](README.md)。h3x 的既定接口、结构和协议行为保持不变。DHTTP 只封装 Endpoint、共享网络、连接复用、应用接入与流适配，不增加传输配额、交换控制、完成订阅或错误缓存。

2026-09-26 按用户要求合并过碎实现并使用普通 `mod`：库根为 `src/dhttp.rs`，`endpoint.rs` 保留 Endpoint/服务接入，`endpoint/messages.rs` 集中请求和 Body 适配。2026-09-27 用户进一步批准可替换的泛型 Network：`network.rs` 只保留通用池和服务驱动，`network/quic.rs` 与 `network/tcp.rs` 分别负责后端；`transport/quic.rs` 与 `transport/tcp.rs` 分别适配 h3x 的流接口。条件编译仅选择后端模块。build.rs 生成的配置常量继续通过生成文件导入。

## 1. 边界与现成类型

- Endpoint 只持规范化名称；独立 load 不访问 Network。
- 同规范化名称代表同一逻辑 Endpoint。同名句柄不区分 load 次数，经 Network 使用同一个本端身份连接池。
- Network 在进程内初始化一次；负责实际连接、监听登记和后台任务。
- Endpoint 不提供 close 或 stop_listening；同名出站请求继续复用连接。
- Network 属于进程生命周期，不提供 shutdown；应用退出时回收自己的任务，监听 future 随运行时退出而结束。
- Pishoo 的 Lib 出站只通过当前身份的 Endpoint；反代直接连接本机 HTTP/TCP 服务，不调用 dhttp Endpoint，也不在两条路径之间回退。
- 用户批准 `tcp-mock` 编译特性和 Network 泛型化：默认后端为 QuicTransport；测试后端为 TcpTransport，通过单条回环 TCP 连接复用 h3x 的双向请求流与单向控制/QPACK 流。独立进程的客户端仍调用 Endpoint，标准 HTTP 请求与响应继续经过 h3x。TCP mock 不验证 QUIC、TLS 对端认证或路径发现。
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
type BoxService = tower::util::BoxCloneService<
    http::Request<Body>, http::Response<Body>, BoxError,
>;
type ConnectionKey = (Arc<str>, Arc<str>); // 本端规范化名称、远端规范化名称
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

## 3. Network：全部实际资源

```rust
pub struct DhttpNetwork {
    network: Network<ActiveTransport>,
}
struct Network<T: h3x::Transport> {
    listeners: Mutex<HashMap<Arc<str>, active::ListenerEntry>>,
    pool: h3x::Pool<ConnectionKey, T, Error>,
    backend: active::BackendState,
}
impl DhttpNetwork {
    pub async fn init() -> Result<&'static Self>;
    pub fn global() -> Result<&'static Self>;
}

mod quic {
    struct BackendState {
        bindings: Mutex<HashMap<(String, IpAddr), Binding>>,
        addresses: qprotocol::AddressBook,
    }
    struct Binding {
        socket: Arc<qprotocol::UdpSocket>,
        scopes: Scopes,
        device_index: u32,
    }
}
mod tcp { struct BackendState; } // 监听 socket 由 listen future 持有
```

泛型 Network 持有现成的 h3x Pool，按本端与远端规范化名称组成的键复用连接。同名 Endpoint 的请求在 await 时访问同一个池，不在 Endpoint 或 Request 中另存连接池。每次构建只选择一种后端，不能在一次请求失败后降级到另一种后端。默认 QUIC 连接供不同请求并发开启独立双向流；测试后端在 TCP 帧上分发相同的 h3x 流 ID 和关闭信号。入站匿名连接不进入复用池，其接入 driver 持有实际连接并负责退出时释放。

Network 不保存 TaskTracker 或全局取消 token。listen future 持有本次监听；读写 driver 持有实际连接/原生流，在各自退出分支释放资源。确需等待并发子任务时，由该操作的局部 JoinSet 负责，不建立全局任务账本。Network 只初始化一次，保持到进程退出；Pishoo `run` 返回不表示监听、池与后台维护任务已关闭。

QUIC BackendState 的 Binding 没有额外方法或 Drop 机制，实际地址直接读取 socket.local_addr()，不另存副本。QUIC 网络维护任务根据当前监听更新 AddressBook/协议/Dock 登记；当没有监听范围时撤销对应绑定。TCP 后端只绑定 `DHTTP_TCP_MOCK_PORTS` 显式指定的回环端口。

超时和流缓冲使用模块内部常量：CONNECT_TIMEOUT、OPERATION_TIMEOUT、BODY_WINDOW_BYTES、BODY_READ_CHUNK_BYTES。它们只规定单次操作或局部缓冲，不构成全局/逐 Endpoint 配额。普通请求与 exec 的业务期限仍由 Pishoo 决定。

## 4. 应用服务接入

```rust
struct ListenerEntry {
    service: BoxService,
    scopes: Scopes,
}
// tcp-mock 构建在 network/tcp.rs 中使用：
type ListenerEntry = BoxService;
```

上方第一个 ListenerEntry 属于默认 QUIC 后端；scopes 用于当前接口绑定的范围并集。tcp-mock 直接登记 BoxService。没有 ListenerPhase、ListenGuard、ServiceAdapter 或监听专用状态机。

为了让同一服务表接收不同具体类型的 Router/handler，直接组合现成 Tower 工具：map_err 把 S::Error 转为 BoxError，map_response 把响应 body 装箱，boxed_clone 得到 BoxService。每请求克隆实例后通过现成 oneshot 或 ready+call 驱动同一个实例，不把 readiness 和 call 分给不同克隆。

默认 QUIC 后端先异步读取凭据，再锁住 listeners 检查名称未被占用；持锁调用同步 qconn.listen 并登记 Service。listen future 接收连接并服务请求，局部 scopeguard 在 future 结束或被丢弃时撤销登记。错误直接由 listen 的 Result 交付，不缓存结束状态。

tcp-mock 后端先绑定 `DHTTP_TCP_MOCK_PORTS` 中本名称对应的回环端口，再登记 Service；每个被接受的 TCP 连接构造一个 TcpTransport 与 H3Connection，随后使用相同的 serve_connection/serve_exchange。监听 future 结束或被丢弃时撤销登记，不改变其他同名 Endpoint 句柄。

## 5. 连接复用与进程生命周期

请求从 Network 的 Pool 按本端与远端名称复用或建立连接。同名 Endpoint 不保存独立的池、关闭状态或连接集合。Network 没有全局关闭阶段；已取得的连接由请求或接入 driver 持有，监听 future 结束不取消现有出站通信。

Pishoo 退出时关闭并等待自己的应用任务。监听 future、Network 复用池、其他在途通信与周期性接口维护任务仍属进程范围，直至进程退出；`run` 返回不保证这些资源已结束。

listeners 锁内不 await、不调用应用 Service。

连接失效或收到 GOAWAY 时，h3x Pool 按自身复用规则换连接。请求 future 被放弃后的底层建连任务能否立即停止取决于 qconn 的既有取消契约；不能把应用 future 的丢弃等同于全局传输关闭。没有逐身份终态，不需要 generation 或 OwnerKey。

listen 按本次 Result 交付错误，不缓存全局关闭报告。driver 在自身操作的所有权下回收资源，不增加独立的结束或失败订阅。监听 future 的结束不会清空连接池或取消出站请求。

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
// 仅 tcp-mock：
type BiStream = (u64, (TcpReader, TcpWriter));
type UniStream = (u64, TcpReader);
struct TcpTransport {
    handshake: Arc<qtls::HandshakeSummary>,
    role: h3x::Role,
    next_bi: AtomicU64,
    next_uni: AtomicU64,
    outgoing: mpsc::Sender<WireFrame>,
    register: mpsc::UnboundedSender<(u64, DuplexStream)>,
    incoming_bi: Mutex<mpsc::UnboundedReceiver<BiStream>>,
    incoming_uni: Mutex<mpsc::UnboundedReceiver<UniStream>>,
    closed: watch::Sender<bool>,
}
impl TcpTransport {
    fn new(socket: tokio::net::TcpStream, role: h3x::Role,
        handshake: Arc<qtls::HandshakeSummary>) -> Self;
}
struct TcpReader { io: Option<DuplexStream>, id: u64, outgoing: mpsc::Sender<WireFrame> }
struct TcpWriter { io: Option<DuplexStream>, id: u64, outgoing: mpsc::Sender<WireFrame> }
enum WireFrame { OpenBi(u64), OpenUni(u64), Data(u64, Bytes), Fin(u64), Reset(u64), Close }
```

保留已有 Transport/AsyncRead/AsyncWrite/StopSending/CancelStream/TransportError 实现；不增加自定义关联方法、成员或 Drop 行为。这些既有 trait 的方法签名保持原样。QuicTransport 直接转发 native 开流、接流和 close；Recv/Send 直接转发 I/O 与取消，不增加广播、统计或完成状态。

TcpTransport 仅测试构建启用。一条 TCP socket 上以类型、h3x 流 ID、长度分帧；有界通道把每个逻辑流的字节送入 h3x 所需的双向或单向 AsyncRead/AsyncWrite。客户端与服务端在不同进程，不共享流句柄或监听登记。它模拟流开启、DATA、FIN、取消及关闭，不能代表 QUIC 的拥塞控制、RESET 错误码、TLS 验证或路径发现。测试 HandshakeSummary 使用本端证书能力且 remote=None。

TCP mock 的 `TcpReader::stop` 仅关闭本端读取方向。单个 `Reset(stream_id)` 帧指向对端接收方向，不能拿它表示 STOP_SENDING，否则拒绝请求体时会误断同一双向流的响应；完整的对端停止发送和错误码传播仍属于本 mock 未覆盖的 QUIC 语义。

DHTTP 对每条已建立连接只启动一次请求接入循环；可复用的出站连接由 Network 的池持有，入站连接由接入 driver 持有。连接接受请求时取得当前名称的监听 Service，没有有效监听则拒绝本次请求，不关闭仍用于其他请求的连接。

## 7. Body 适配没有补建协议能力

Body 只是标准 UnsyncBoxBody 别名，没有成员、方法、错误缓存或 consumed channel。h3x 原生消息已经负责 HTTP 消息、body、trailers、EOF、读写错误和 stop/cancel。DHTTP 的转换只为连接已有接口：

- 接收：在局部 StreamBody/async stream 中读原生数据，产生 Frame::data；数据 EOF 后交付已有 trailers。错误直接返回，之后不伪造 trailers。
- 提前放弃：进入生成器前建立现成 scopeguard，持有原生接收方向；正常 EOF 后解除 guard，提前 Drop 调用已有 stop。无需自定义 Guard 类型。
- 发送：标准 Body 的 DATA/trailers 写入原有 h3x 可写消息；正常 EOF 调用 shutdown，失败调用既有 cancel；与固定 h3x writer future 并发推进。
- HEAD/204/304 等消息语义沿用 h3x。DHTTP 丢弃被抑制的应用 Body；Pishoo 的 guest 不因 Body 丢弃而被单独取消，后续 I/O 错误或 Lib 关闭决定其退出。

适配所需的纯模块内函数、闭包和 async 局部变量属于方法实现，不列成另一套冻结公共函数。标准适配不会反向要求 h3x 增加新的消息类型或生命周期接口。

操作超时只作用于当前可观察的等待阶段，不声称观察到 h3x 未公开的 native 进度；不能给整个长流 writer future 套短总时长限制。exec 的固定执行期限由 Pishoo 自己落实。

## 8. 身份与签名接缝

```rust
pub fn subject_id(certificates: &[qtls::CertificateDer<'_>]) -> Result<Vec<u8>>;
pub fn sign(local: &qtls::LocalAuthority, data: &[u8]) -> Result<Vec<u8>>;
pub fn verify_signature(spki: &[u8], data: &[u8], signature: &[u8]) -> Result<bool>;
pub async fn resolve_remote(endpoint: &Endpoint, name: &str) -> Result<qtls::RemoteAuthority>;
```

这四个 certificate 函数是 Pishoo 的跨仓接缝，保留冻结。subject_id 沿现有 DHTTP SKI owner_hash 文本字节规范；sign/verify 复用既有规范算法；resolve_remote 取得实际握手验证的对端，不承诺离线或历史证书查询。凭据读取和信任装配继续复用现有 home/trust 内部代码，不新建身份结构。

成功入站先依据握手本端身份展开 URI authority 简写，并核对规范化 authority 的 host 与该身份一致；缺少本端身份、authority 或身份不匹配时直接返回 421，不调用应用 Service。authority 可带 `:序号` 后缀，作为将来与本端证书 DHTTP SKI 中 chain sequence 核对的地址信息；本版保留原值，不将它用作传输端口，也暂不校验该序号。随后把实际 HandshakeSummary 放入 request extensions；缺少摘要是接入错误，remote=None 才表示匿名。LocalAuthority 的签名能力留在可信宿主，guest 只经 Pishoo 授权的接口使用。出站忽略转带的可信身份 extensions，使用当前 Endpoint 的身份。

## 9. 固定调用关系

1. Endpoint.load → 名称规范化；get/post/request/header/body → 请求字段构造，不访问 Network。
2. Request.await → global Network → 检查名称状态 → Pool 取得连接 → open_bi → 并发 write_request/read_response。
3. Endpoint.listen → 准备凭据 → 原子登记 → supervisor → accept_bi → read_request → 标准 Service → write_response。
4. Service/代理/WASI 使用标准 Body；适配器只桥接数据、trailers 与原有流结束语义。
5. listen future 结束时撤销本次监听登记；剩余全局资源随进程退出结束。

没有 OwnerKey、自定义 ConnectionKey 结构、Phase、NetworkState、ListenerPhase、ServiceAdapter、ListenGuard、ShutdownReport、ExchangeLease 或精细关闭计数。默认后端保留 Endpoint、Request、DhttpNetwork、泛型 Network、ListenerEntry、BackendState、Binding、QuicTransport、RecvStream、SendStream；tcp-mock 额外使用上列 TcpTransport、TcpReader、TcpWriter、WireFrame 与本后端 ListenerEntry。Error 沿用现有类型。

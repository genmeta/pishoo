# dhttp 第一版结构与接口清单

本清单定义当前设计；遵循[清单约束](README.md)。h3x 的既定接口、结构和协议行为保持不变。DHTTP 只封装 Endpoint、共享网络、连接复用、应用接入与流适配，不增加传输配额、交换控制、完成订阅或错误缓存。

2026-09-26 按用户要求合并过碎实现并使用普通 `mod`：库根为 `src/dhttp.rs`，`endpoint.rs` 保留原资源声明并集中 Endpoint/连接/服务接入，`endpoint/network.rs` 集中 Network 生命周期与接口绑定，`endpoint/messages.rs` 集中请求和 Body 适配。子模块私有，既有无状态 helper 按调用需要使用 `pub(super)`；公开重导出路径、类型字段和调用行为保持不变。build.rs 生成的配置常量继续通过生成文件导入，不承担手写实现的模块拆分。

## 1. 边界与现成类型

- Endpoint 只持规范化名称；独立 load 不访问 Network。
- 同规范化名称代表同一逻辑 Endpoint。同名句柄不区分 load 次数，经 Network 使用同一个本端身份连接池。
- Network 在进程内初始化一次；负责实际连接、监听登记和后台任务。
- Endpoint 不提供 close；stop_listening 仅停止当前监听，随后可以重新 listen，同名出站请求继续复用连接。
- Network 属于进程生命周期，不提供 shutdown；应用退出时逐个停止其 Endpoint 监听并回收自己的任务。
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
    listeners: Mutex<HashMap<Arc<str>, ListenerEntry>>,
    pool: h3x::Pool<ConnectionKey, QuicTransport, Error>,
    bindings: Mutex<HashMap<(String, IpAddr), Binding>>,
    addresses: qprotocol::AddressBook,
}
impl DhttpNetwork {
    pub async fn init() -> Result<&'static Self>;
    pub fn global() -> Result<&'static Self>;
}

struct Binding {
    socket: Arc<qprotocol::UdpSocket>,
    scopes: Scopes,
    device_index: u32,
}
```

Network 持有现成的 h3x Pool，按本端与远端规范化名称组成的键复用连接。同名 Endpoint 的请求在 await 时访问同一个池，不在 Endpoint 或 Request 中另存连接池。连接供不同请求并发开启独立双向流；不可复用的连接由 h3x Pool 按既有规则替换。入站匿名连接不进入复用池，其接入 driver 持有实际连接并负责退出时释放。

Network 不保存 TaskTracker 或全局取消 token。listen 自己等待其 supervisor 的 JoinHandle；读写 driver 持有实际连接/原生流，在各自退出分支释放资源。确需等待并发子任务时，由该操作的局部 JoinSet 负责，不建立全局任务账本。Network 只初始化一次，保持到进程退出；Pishoo `run` 返回不表示池与后台维护任务已关闭。

Binding 没有额外方法或 Drop 机制，实际地址直接读取 socket.local_addr()，不另存副本。网络维护任务根据当前监听更新 AddressBook/协议/Dock 登记；当没有监听范围时撤销对应绑定。设备和地址变化重新应用当前有效监听登记的范围。

超时和队列容量使用模块内部常量：CONNECT_TIMEOUT、OPERATION_TIMEOUT、BODY_WINDOW_BYTES、BODY_READ_CHUNK_BYTES、ACCEPT_QUEUE_CAPACITY。它们只规定单次操作或局部缓冲，不构成全局/逐 Endpoint 配额。普通请求与 exec 的业务期限仍由 Pishoo 决定。

## 4. 应用服务接入

```rust
struct ListenerEntry {
    service: Mutex<ErasedService>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    owner: Arc<qconn::Server>,
    scopes: Scopes,
}
```

没有 ListenerPhase、ListenGuard、ServiceAdapter 或监听专用状态机。name 已是 Network 表键；scopes 用于当前接口绑定的范围并集，shutdown sender 的存在表示本次监听仍有效。

为了让同一服务表接收不同具体类型的 Router/handler，直接组合现成 Tower 工具：map_err 把 S::Error 转为 BoxError，map_response 把响应 body 装箱，boxed_clone 得到 ErasedService。每请求克隆实例后通过现成 oneshot 或 ready+call 驱动同一个实例，不把 readiness 和 call 分给不同克隆。

监听提交顺序：

1. 先异步读取并验证凭据，期间尚未占用监听名称；随后创建供本次监听使用的局部 oneshot 通道。
2. 锁住 listeners，检查名称尚未监听且 qconn 注册表没有同名登记。
3. 持锁调用同步 qconn.listen，取得实际 Arc<qconn::Server> 后构造完整 ListenerEntry 并插入表，再启动本次 supervisor。锁本身保证同名登记互斥，不创建占位或空 OnceLock。
4. 提交临界区中没有 await。同步失败立即返回错误；已取得的底层登记在返回前撤销。不留下 Preparing 状态或半完成的登记。
5. listen future 局部持一个 oneshot Sender；supervisor 持 Receiver。Sender 的生存期明确跨越整个监听等待，future 被丢弃时通道关闭，supervisor 据此停止并撤登记。
6. 公共 listen 等待本次 supervisor 的 JoinHandle 返回 Result<()>；错误直接沿返回链交付，不缓存到登记字段。

ListenerEntry.shutdown 是本次监听的一次性停止 sender；supervisor 观察对应 receiver 和调用方 future 的局部 oneshot。它的真实 qconn 撤销和任务退出负责取消路径收尾，不能把必须执行的清理只写在 public listen future 的正常返回路径。

stop_listening 取走当前登记的 shutdown sender 并发送停止信号，在 listeners 锁内按 owner 的 Arc 指针撤销真实 qconn 登记；当前无登记时返回 Ok(())。supervisor 随后移除旧表项并回收已接受连接。stop_listening 不等待旧任务结束，也不关闭连接池中的出站连接。

qconn 接收回调捕获本次监听名称。接受结果经固定有界交接队列交 supervisor；队列满或接收方消失时立即关闭该连接。回调在 listeners 锁下要求 shutdown sender 仍存在；stop_listening 先取走 sender，使旧回调不能接到新登记。迟到结果直接关闭。supervisor 清理本次 qconn owner。

## 5. 连接复用与进程生命周期

请求从 Network 的 Pool 按本端与远端名称复用或建立连接。同名 Endpoint 不保存独立的池、关闭状态或连接集合。Network 没有全局关闭阶段；已取得的连接由请求或接入 driver 持有，服务停止监听不取消现有出站通信。

Pishoo 退出时调用各 Endpoint 的 stop_listening，并取消和等待自己的应用任务。Network 的复用池、其他在途通信与周期性接口维护任务仍属进程范围，直至进程退出；`run` 返回不保证这些资源已结束。

listeners 锁内不 await、不调用应用 Service；停止的监听不得再接收新交换。

连接失效或收到 GOAWAY 时，h3x Pool 按自身复用规则换连接。请求 future 被放弃后的底层建连任务能否立即停止取决于 qconn 的既有取消契约；不能把应用 future 的丢弃等同于全局传输关闭。没有逐身份终态，不需要 generation 或 OwnerKey。

各次 stop_listening 按本次 Result 交付错误，不缓存全局关闭报告。driver 在自身操作的所有权下回收资源，不增加独立的结束或失败订阅。

stop_listening 只撤销当前监听，不清空池或取消出站请求。停止监听后可重新 listen，也仍能主动发请求；已接入交换可以继续。服务删除和组件更新使用 stop_listening/Router 更新，同名服务可重新接入。

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

DHTTP 对每条已建立连接只启动一次请求接入循环；可复用的出站连接由 Network 的池持有，入站连接由接入 driver 持有。连接接受请求时取得当前名称的监听 Service，没有有效监听则拒绝本次请求，不关闭仍用于其他请求的连接。

## 7. Body 适配没有补建协议能力

Body 只是标准 UnsyncBoxBody 别名，没有成员、方法、错误缓存或 consumed channel。h3x 原生消息已经负责 HTTP 消息、body、trailers、EOF、读写错误和 stop/cancel。DHTTP 的转换只为连接已有接口：

- 接收：在局部 StreamBody/async stream 中读原生数据，产生 Frame::data；数据 EOF 后交付已有 trailers。错误直接返回，之后不伪造 trailers。
- 提前放弃：进入生成器前建立现成 scopeguard，持有原生接收方向；正常 EOF 后解除 guard，提前 Drop 调用已有 stop。无需自定义 Guard 类型。
- 发送：标准 Body 的 DATA/trailers 写入原有 h3x 可写消息；正常 EOF 调用 shutdown，失败调用既有 cancel；与固定 h3x writer future 并发推进。
- HEAD/204/304 等消息语义沿用 h3x。DHTTP 处理被抑制的应用 producer，Pishoo 回收自己的应用任务。

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

成功入站把实际 HandshakeSummary 放入 request extensions；缺少摘要是接入错误，remote=None 才表示匿名。LocalAuthority 的签名能力留在可信宿主，guest 只经 Pishoo 授权的接口使用。出站忽略转带的可信身份 extensions，使用当前 Endpoint 的身份。

## 9. 固定调用关系

1. Endpoint.load → 名称规范化；get/post/request/header/body → 请求字段构造，不访问 Network。
2. Request.await → global Network → 检查名称状态 → Pool 取得连接 → open_bi → 并发 write_request/read_response。
3. Endpoint.listen → 准备凭据 → 原子登记 → supervisor → accept_bi → read_request → 标准 Service → write_response。
4. Service/代理/WASI 使用标准 Body；适配器只桥接数据、trailers 与原有流结束语义。
5. stop_listening → 撤当前监听，保留连接复用；剩余全局资源随进程退出结束。

没有 OwnerKey、自定义 ConnectionKey 结构、Phase、NetworkState、ListenerPhase、ServiceAdapter、ListenGuard、ShutdownReport、ExchangeLease 或精细关闭计数。保留的自有结构只有 Endpoint、Request、DhttpNetwork、ListenerEntry、Binding、QuicTransport、RecvStream、SendStream；Error 沿用现有类型。

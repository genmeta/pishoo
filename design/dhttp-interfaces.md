# dhttp 第一版结构与接口清单

本清单定义当前设计；遵循[清单约束](README.md)。h3x 的既定接口、结构和协议行为保持不变。DHTTP 只封装 Endpoint、共享网络、连接复用、应用接入与流适配，不增加传输配额、交换控制、完成订阅或错误缓存。

2026-10-02 按用户指定的 [Network 详细设计](../../dhttp/docs/design/network-detailed-design.md)重构：QUIC 为唯一传输，Network 逻辑统一在 `network.rs`，流适配保留在 `transport/quic.rs`。删除泛型后端包装、TCP mock 和绑定状态镜像。库根保持 `src/dhttp.rs`，使用普通 `mod`。

## 1. 边界与现成类型

- Endpoint 持已加载的 `Arc<qconn::QuicEndpoint>`，独立 load 不访问 Network；签名与 QUIC 共用内存凭据。
- 同规范化名称代表同一逻辑 Endpoint。同名句柄不区分 load 次数，经 Network 使用同一个本端身份连接池。
- Network 通过幂等 init 在进程内装配一次；准备全部可用网卡并监听变化，负责连接复用与服务登记。
- Endpoint 不提供 close 或 stop_listening；同名出站请求继续复用连接。
- Network 属于进程生命周期，不提供 shutdown；应用退出时回收自己的任务，监听 future 随运行时退出而结束。
- Pishoo 暂不提供 Lib 出站；配置反代直接连接本机 HTTP/TCP 服务，不调用 dhttp Endpoint。另有同名身份专用的固定前缀 DHTTP 正向代理，复用当前 Server 已有的 Endpoint 并保持纯转发；dhttp 出站响应在进程内携带 RemoteAuthority，供发起业务请求的调用方使用。
- Pishoo 只等待自己的应用任务；DHTTP 不提供 finished、ExchangeControl、RequestInfo 或 Peer。

直接复用 http/http-body、http-body-util、Tower、Tokio、tokio-util、async-stream 和 scopeguard。以下签名省略这些现成类型的 use 声明。

```rust
pub use qtls::{HandshakeSummary, LocalAuthority, RemoteAuthority, CertificateDer};
pub use qconn::{Scope, Scopes};
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type Result<T> = std::result::Result<T, Error>;
pub type EmptyBody = http_body_util::Empty<Bytes>;
pub type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, BoxError>;
pub type ListenFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
pub type RequestFuture = Pin<Box<
    dyn Future<Output = Result<http::Response<Body>>> + Send + 'static,
>>;
type BoxService = tower::util::BoxCloneService<
    http::Request<Body>, http::Response<Body>, BoxError,
>;
#[derive(Clone, Debug)]
enum ConnectionKey {
    Incoming {
        local: Arc<str>,
        remote: Option<Arc<str>>,
    },
    Outgoing {
        local: Option<Arc<str>>,
        remote: Arc<str>,
    },
}

impl ConnectionKey {
    fn names(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Self::Incoming { local, remote } => (Some(local), remote.as_deref()),
            Self::Outgoing { local, remote } => (local.as_deref(), Some(remote)),
        }
    }
}

// The same named peers share a pool entry regardless of who initiated the connection.
impl PartialEq for ConnectionKey {
    fn eq(&self, other: &Self) -> bool {
        self.names() == other.names()
    }
}

impl Eq for ConnectionKey {}

impl Hash for ConnectionKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.names().hash(state);
    }
}
static NETWORK: tokio::sync::OnceCell<DhttpNetwork> = tokio::sync::OnceCell::const_new();
```

Error 复用现有错误类型；失败通过 Result、流错误或任务返回传播。需要表达互斥的可用/失败结果时，使用 Result<可用对象, Error>，不在正常对象旁另存 error、failed、closed 标记。不引入独立 Response future 结构或 ShutdownReport。

## 2. Endpoint 与可 await 请求

```rust
#[derive(Clone)]
pub struct Endpoint { pub(crate) quic: Arc<qconn::QuicEndpoint> }
impl Endpoint {
    pub async fn load(name: impl AsRef<str>) -> Result<Self>;
    pub fn name(&self) -> &str;
    pub fn local_authority(&self) -> Result<qtls::LocalAuthority>;
    pub fn get(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn head(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn post(&self, uri: http::Uri) -> Request<dhttp::WndBuf>;
    pub fn put(&self, uri: http::Uri) -> Request<dhttp::WndBuf>;
    pub fn patch(&self, uri: http::Uri) -> Request<dhttp::WndBuf>;
    pub fn delete(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn options(&self, uri: http::Uri) -> Request<EmptyBody>;
    pub fn request(&self, method: http::Method, uri: http::Uri) -> Request<dhttp::WndBuf>;
    pub fn from_request<B>(&self, request: http::Request<B>) -> Request<B>;
    pub async fn listen<S, B>(&self, scopes: Scopes, service: S) -> Result<ListenFuture>
    where
        S: tower_service::Service<http::Request<Body>, Response = http::Response<B>>
            + Clone + Send + 'static,
        S::Future: Send + 'static,
        S::Error: Into<BoxError>,
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>;
}

pub struct Request<B> {
    endpoint: Option<Endpoint>,
    message: http::Request<B>,
    expected_remote_owner_hash: Option<dhttp_home::certificate::OwnerHash>,
}
impl<B> Request<B> {
    pub fn new(message: http::Request<B>) -> Self; // 匿名出站
    pub fn expect_remote_owner_hash(self, owner_hash: dhttp_home::certificate::OwnerHash) -> Self;
    pub fn header(self, name: http::HeaderName, value: http::HeaderValue) -> Self;
    pub fn append_header(self, name: http::HeaderName, value: http::HeaderValue) -> Self;
    pub fn body<T>(self, body: T) -> Request<T>;
    pub fn write(self, data: impl AsRef<[u8]>) -> Request<dhttp::WndBuf>;
}
// 现行底层既有调用方式，接入时保持不变。
impl std::future::IntoFuture for Request<EmptyBody> {
    type Output = Result<http::Response<Body>>;
    type IntoFuture = RequestFuture;
    fn into_future(self) -> Self::IntoFuture;
}
impl std::future::IntoFuture for Request<dhttp::WndBuf> {
    type Output = Result<(dhttp::RequestWriter, RequestFuture)>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'static>>;
    fn into_future(self) -> Self::IntoFuture;
}
```

2026-10-03 用户批准 Workspace/Chat 接入所需的最小变更：`Request.expected_remote_owner_hash`、`Request::expect_remote_owner_hash` 和无载荷错误变体 `Error::RemoteIdentityChanged`。默认不钉住 owner_hash；指定时，从 Network 取得实际 H3 连接后、开流及发送 HTTP 头/Body 前，从该连接已验证的 RemoteAuthority 证书提取 SKI 并核对 owner_hash；缺少身份、无效 SKI 或不匹配均返回该错误。复用连接（包括反向连接）也逐请求校验，不另建连接池键或身份缓存。body/write 更换 Body 时保留期望身份；没有发送后补救或先探测再发消息的路径。

Endpoint::from_request 绑定当前身份，Request::new 创建匿名请求；匿名出站不读本地身份文件，具名凭据失败不回退匿名。匿名请求可用 bob~，单独的 ~ 因没有本端名称而返回参数错误。URL 和 header 先解析为有效类型，构造请求时不保存待报错状态。首次轮询 await 才开始网络操作；header/body 构造不需要 Network。Request 不 Clone；发送后不再向调用方交付可编辑头字段的 builder。

2026-10-03 接入当前底层时，保留它已有的两种调用：`Request<Empty>` await 直接返回响应，`Request<WndBuf>` await 返回 `(RequestWriter, RequestFuture)`，由调用方显式 shutdown 上传。用户曾批准增加 `Request<Body>` await，随后在底层任务明确要求删除该分支，现以删除后的接口为准。原来笼统的 `impl<B>` await 记法改为上述两个具体接缝；Body 继续用于响应和 Service 接收端。WndBuf/RequestWriter 当前没有出站请求 trailers 设置接口；用户批准 Pishoo 本轮先做字节流转发，并明确拒绝请求 trailers，响应 trailers 保留。

```rust
let uri: http::Uri = url.parse()?;
let response = endpoint.get(uri)
    .header(http::header::ACCEPT, http::HeaderValue::from_static("application/json"))
    .await?;
```

上传与响应读取并发进行，返回响应头不等待完整上传。from_request 与 fluent 请求走相同内部驱动；没有额外公开 execute/send。

## 3. Network：两个实际成员

```rust
pub struct DhttpNetwork {
    listeners: Mutex<HashMap<Arc<str>, BoxService>>,
    pool: h3x::Pool<ConnectionKey, QuicTransport, Error>,
}
impl DhttpNetwork {
    pub async fn init() -> Result<&'static Self>;
    pub fn global() -> Result<&'static Self>;
    pub(crate) async fn get_connection(&'static self, local: Option<Arc<str>>, remote: Arc<str>)
        -> Result<h3x::H3Connection<QuicTransport>>;
    pub(crate) async fn listen(&'static self, endpoint: &Endpoint, scopes: Scopes, service: BoxService)
        -> Result<ListenFuture>;
}
```

同名 Endpoint 按本端、远端名称值复用 h3x Pool。Incoming 的本端名称必填、远端可选；Outgoing 的远端名称必填、本端可选，不能同时缺少两端身份。Eq/Hash 统一比较本端和远端名称，双方具名且名称相同时，入站、出站键匹配同一池条目。Network 不保存 socket、网卡快照、scopes 副本、凭据缓存、任务集合或出站标志；不保留 BackendState、Binding、ListenerEntry 或泛型 Network。

`OnceCell::get_or_try_init` 协调并发初始化。唯一 netwatcher 先交付初始完整快照，再由一个任务等待系统变化；每次使用同一 scan 对照 Dock 的实际绑定。匹配绑定保留端口，缺失绑定添加，已失效的设备绑定删除；临时打洞 socket 不受扫描接管。没有可用网卡或部分绑定失败仍可初始化，失败绑定只在下次网卡变化后重试。初始化不等待 STUN 探测。普通启动为每个适用的新 socket 自动装配一次性 NAT 分类，之后持续 STUN 绑定心跳维护公网映射；不定期重做 NAT 分类。

Dock 持有 socket 登记、收包任务及配套 AddressBook 引用，负责直接 QUIC/STUN 登记、地址发布、回滚和清理；Network 不保存第二份资源表。收包失败由 Dock 清理；旧路径按发送失败退出，新地址沿用 dquic 的打洞订阅。

重复 init 返回同一个 Network；信任装配或 watcher 创建失败可重试。不提供 NetworkConfig、init_with、shutdown 或独立初始化锁。Tokio runtime 必须持续存活。等待 Pool 的期限保持 30 秒；dhttp 不新增单次开流、消息头或 Body 读写期限。

## 4. 应用服务接入

listeners 只存 BoxService。scopes 原样交给 qconn 的 ServerRegistry，逐名称限制来源，不决定全局 socket 集合。

用 Tower 的 map_err、map_response 和 boxed_clone 统一 Service；每个请求克隆 Service 后由同一实例完成 readiness 和 call。listen 使用 Endpoint 的已加载身份材料，在 listeners 锁内检查名称、登记 qconn 和 Service；scopeguard 在同一锁内撤销两张表。锁内不 await、不调用应用。

接入回调直接装配 H3、入池并启动请求驱动，登记阶段成功返回 ListenFuture，其以 pending 等待取消，scopeguard 负责撤销名称和 Service。guard 在构造 async future 前创建，未 poll 的 future 直接 Drop 也会撤销登记。取消监听保留已有请求、出站连接和 socket；单次握手或 H3 装配失败只结束本次回调。入站使用 Incoming，出站按 Outgoing 查池；双方具名且名称相同时，Eq/Hash 让出站复用已有入站连接。匿名入站不能满足指定远端名称的请求。每条连接仅启动一个请求接入循环，退出时按 key 和实际连接移除。

## 5. 连接复用与进程生命周期

请求从 Network 的 Pool 按本端与远端名称复用或建立连接。同名 Endpoint 不保存独立的池、关闭状态或连接集合。Network 没有全局关闭阶段；已取得的连接由请求或接入 driver 持有，监听 future 结束不取消现有出站通信。

Pishoo 退出时关闭并等待自己的应用任务。监听 future、Network 复用池、其他在途通信与等待系统网卡事件的维护任务仍属进程范围，直至进程退出；`run` 返回不保证这些资源已结束。

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
impl QuicTransport {
    fn new(connection: qconn::ArcConnection, local: Option<qtls::LocalAuthority>,
        remote: Option<qtls::RemoteAuthority>, role: h3x::Role) -> Result<Self>;
}
```

构造时校验实际 ALPN 为 h3，随后直接调用已有 H3Connection::new。qconn 两端使用全局默认 h3 ALPN，QuicEndpoint.identity/new 接受 Option<Arc<qbase::endpoint::Endpoint>>；匿名 connect 省略本地证书，listen 仍要求本端身份。角色来自 connect/listen 接入路径，HandshakeSummary 保留实际本端/对端身份和协商结果。

Transport/AsyncRead/AsyncWrite/StopSending/CancelStream/TransportError 继续转接原生连接和流。装配失败沿用既有连接 Drop 语义；无 Service 时对请求流同时 stop/cancel，连接上的其他交换继续。

DHTTP 对每条已建立连接只启动一次请求接入循环；可复用的出站连接由 Network 的池持有，入站连接由接入 driver 持有。连接接受请求时取得当前名称的监听 Service，没有有效监听则拒绝本次请求，不关闭仍用于其他请求的连接。

## 7. Body 适配没有补建协议能力

Body 只是标准 UnsyncBoxBody 别名，没有成员、方法、错误缓存或 consumed channel。h3x 原生消息已经负责 HTTP 消息、body、trailers、EOF、读写错误和 stop/cancel。DHTTP 的转换只为连接已有接口：

- 接收：在局部 StreamBody/async stream 中读原生数据，产生 Frame::data；数据 EOF 后交付已有 trailers。错误直接返回，之后不伪造 trailers。
- 提前放弃：进入生成器前建立现成 scopeguard，持有原生接收方向；正常 EOF 后解除 guard，提前 Drop 调用已有 stop。无需自定义 Guard 类型。
- 发送：标准 Body 的 DATA/trailers 写入原有 h3x 可写消息；正常 EOF 调用 shutdown，失败调用既有 cancel；与固定 h3x writer future 并发推进。
- 出站上传：由现行 RequestWriter 独立持有；丢弃未完成的 writer 取消上传，shutdown 结束上传并等待发送完成。响应直接使用 h3x 原生 Body，不通过响应读取或丢弃额外控制上传。不恢复 Request<Body> 分支、send_body_request 或响应 Body 包装。
- HEAD/204/304 等消息语义沿用 h3x。DHTTP 丢弃被抑制的应用 Body；Pishoo 的 guest 不因 Body 丢弃而被单独取消，后续 I/O 结果或 guest 自身执行决定其退出。

适配所需的纯模块内函数、闭包和 async 局部变量属于方法实现，不列成另一套冻结公共函数。标准适配不会反向要求 h3x 增加新的消息类型或生命周期接口。

dhttp 的读写等待由流背压、EOF、错误和取消推进，不给开流、消息头、Body frame 或原生读写额外套定时器。调用方可取消所持有的请求 future 或丢弃 Body；exec 的固定执行期限由 Pishoo 自己落实。

## 8. 身份与签名接缝

出站请求取得 HTTP 响应后，dhttp 将本次请求实际使用的连接中已验证的 `qtls::RemoteAuthority` 克隆到 `Response.extensions`；没有已验证远端时不插入。扩展只在本进程有效，不作为 HTTP 字段传输。`resolve_remote` 已从现行 dhttp 公开接口删除，不恢复独立查询函数。需要对端 SKI 的应用可从扩展中的证书按 dhttp-home 规则提取 owner_hash；Pishoo 的纯转发函数不消费它，`DhttpContactNotifier::submit_application` 会消费。签名仍用 qtls::LocalAuthority 选择规范算法，验签直接使用 dhttp-home 的规则。凭据读取和信任装配继续复用现有 home/trust 内部代码，不新建身份结构。

成功入站先依据握手本端身份展开 URI authority 简写，并核对规范化 authority 的 host 与该身份一致；缺少本端身份、authority 或身份不匹配时直接返回 421，不调用应用 Service。authority 可带 `:序号` 后缀，作为将来与本端证书 DHTTP SKI 中 chain sequence 核对的地址信息；本版保留原值，不将它用作传输端口，也暂不校验该序号。随后把实际 HandshakeSummary 放入 request extensions；缺少摘要是接入错误，remote=None 才表示匿名。LocalAuthority 的签名能力留在可信宿主，guest 只经 Pishoo 授权的接口使用。出站忽略转带的可信身份 extensions；具名请求使用当前 Endpoint 的身份，Request::new 创建匿名请求且仍验证远端身份。

## 9. 固定调用关系

1. Endpoint.load → 名称规范化；get/post/request/header/body → 请求字段构造，不访问 Network。
2. Request.await → global Network → Pool 取得连接 → open_bi → 并发 write_request/read_response。
3. Endpoint.listen → 准备凭据 → 原子登记 → serve_connection → accept_bi → read_request → 标准 Service → write_response。
4. Service/代理/WASI 使用标准 Body；适配器只桥接数据、trailers 与原有流结束语义。
5. listen future 结束时撤销本次监听登记；剩余全局资源随进程退出结束。

没有 OwnerKey、Phase、NetworkState、ListenerPhase、ServiceAdapter、ListenGuard、ShutdownReport、ExchangeLease 或精细关闭计数。保留 Endpoint、Request、ConnectionKey、DhttpNetwork、QuicTransport、RecvStream、SendStream。Error 沿用现有类型。

2026-10-03 用户批准 DNS 接缝：确认上述现行 Endpoint.quic；local_authority 从它的名称、证书、signing_key 和 OCSP 构造现成 LocalAuthority，不读磁盘、不缓存、不访问 Network。Endpoint.listen/Network.listen 返回已登记的 ListenFuture；调用方先 await 登记再 spawn 生命周期，登记失败不启动发布。Network 成员不变。相邻 qprotocol 的 AddressBook 新增 `pub fn inner_bindings(&self) -> Vec<(SocketAddr, qudp::BoundDevice)>`，只派生有有效 Internal 地址、端口和现有网卡元数据的实际绑定，按 bound 去重并排除 Loopback；不增加成员。完整跨仓 DNS 差异见 [DNS 设计](pishoo-dns-detailed-design.md)。

2026-10-03 用户要求普通启动自动接入 NAT，并批准为既有私有 Binding 增加 `nat_probe: futures::stream::BoxStream<'static, std::io::Result<(qbase::net::NatType, std::net::SocketAddr, std::net::SocketAddr)>>`。该流由唯一 Network 维护任务轮询，先在新 socket 上进行一次 NAT 分类，再每20秒向同地址族 STUN 节点发送绑定心跳维护映射；Loopback/IPv6 link-local 不探测。映射变化时更新 QUIC 直接/中介别名与 AddressBook，失败映射撤回并在后续心跳重试。绑定撤回时丢弃流和未完成 transaction，不建立独立探测任务或第二份绑定表。DhttpNetwork、Endpoint 和 h3x 成员不变，初始化仍不等待 STUN。

2026-10-04 中转 DNS 修复：已有 nat_probe 取得的 agent/outer 不能在写入 AddressBook 时丢弃 agent。Network 继续登记 Direct 与 Mediate QUIC 别名；对 FullCone 的 DDNS 发布采用 Direct，对受限/未知 NAT 采用已有 Mediate 值。AddressBook 的外部插入/替换方法体允许有效 Mediate，内部仍要求 Direct，成员与签名不变；ddns 既有 E-record 编码保持 NAT 标记并输出 outer-agent。内部 qbase EndpointAddr 的 Display 仍为 agent-outer，不改变内部地址语法。

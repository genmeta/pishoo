# Pishoo DNS 解析与发布详细设计

日期：2026-10-03。状态：2026-10-03 用户明确批准全部六项接口与相邻仓库修改，已并入冻结基线；实现与验证进度见 [实施记录](../IMPLEMENTATION.md)。

Pishoo 在进程入口装配名称解析，在监听登记成功后发布身份地址，并在地址变化、记录续期、身份删除和退出时更新记录。协议编码、签名和缓存规则归 ddns；地址、socket、NAT 与连接池继续归 dhttp/dquic。本文逐项定义所需结构、成员、函数及伪代码，按本次批准实施。

## 设计依据与实施前提

依据本仓 [设计入口](README.md)、[dhttp 接口](dhttp-interfaces.md)、[Pishoo 接口](pishoo-interfaces.md)，以及本地 ddns、dquic 和 ddns-sever 当前代码。下表记录实施前的代码状况；本次批准后的接口以结构与签名章节为准。

| 实施前的代码事实 | 对设计的影响 |
| --- | --- |
| qresolve 全局注册表初始为空，System DNS 也是显式注册的源 | Pishoo 必须装配解析器，且不能让 DDNS 查询递归解析 DDNS 服务自身 |
| AddressBook 有 subscribe_ddns、subscribe_mdns、subscribe_punch，保存绑定的网卡元数据 | 使用现有订阅，不新建 watcher 或地址状态表 |
| MdnsResolverSet 已拥有按网卡和 IP 区分的实际 mDNS 实例 | 复用其 upsert、remove、snapshot、shutdown，不新建资源管理结构 |
| H3Resolver 当前持 Endpoint 的 Weak 引用，发布时重新读取身份文件 | 删除身份目录后无法可靠撤回；需复用 Endpoint 已加载的内存凭据 |
| Endpoint.listen 当前在登记完成后持续 pending，await 不交付登记完成结果 | 把登记成功与监听存续通过两层 future 分开表达 |
| DDNS 服务按名称与完整发布者 SKI 清除记录 | 撤回只清除自身凭据对应记录，同名其他凭据保留 |
| 发布响应当前只有 200 和 OK；服务端 TTL 可配置 | 客户端无法正确决定续期时间；响应必须交付实际租期 |
| ddns 编码的 E 记录 TTL 当前固定 300 秒，服务端存储租期可与它不同 | 查询缓存不能直接把 300 秒当作动态记录剩余有效期 |
| mDNS 的 remove_name 当前只删除本机应答记录 | 本版不承诺向已有查询者主动广播撤回，远端已有缓存按 TTL 过期 |

原冻结清单写 Endpoint 只有 name，而现行 dhttp 已使用 `quic: Arc<qconn::QuicEndpoint>` 并在 load 时保存凭据。本次用户明确批准确认此差异，并同步 dhttp 清单；签名使用同一组内存材料，Pishoo 不添加另一份凭据容器。

## 范围与不变量

1. 全进程分别注册现有 SystemResolver、匿名 H3Resolver 和 MdnsResolverSet 各一次；由现有 qresolve::Resolver 并行聚合。各源在 lookup 中筛选自己支持的名称，不新增 Pishoo 组合解析器。
2. 解析不依赖 Server 是否监听。listen 为 0 的身份仍能出站。
3. 发布严格遵循启动时 listen 范围：0 不发布，1 只 mDNS，2 只 H3，3 两者。重载不能改变 listen，保持原有重启要求。
4. 只有成功完成监听登记的 Server 进入发布流程。启动登记失败直接结束启动并收尾已经创建的 DNS 资源。
5. 同一身份同一时刻最多存在一个本进程 H3 发布请求。每批开始时取地址簿最新完整快照，批结束后才创建下一批。
6. Server 删除、重载或进程退出前，当前发布批必须结束；退出之后不再创建续期请求。
7. 发布器使用与它的 Endpoint 相同的内存凭据，编码签名与 TLS 身份不混用新旧证书。
8. Pishoo 不保存 IP 列表、网卡快照、成功发布记录表或逐身份发布状态。不引入 DNS Manager、发布数据库、世代计数、取消 token 或新的应用并发限制。
9. 相同完整身份凭据的多个进程会更新同一服务端记录；本版不承诺隔离它们的撤回。同名多设备应使用各自独立的发布者凭据。
10. DNS 返回的是候选地址；实际可达性、握手身份和路径验证仍归 dquic/qtls。

## 结构与成员

### 解析器资源归属

Pishoo 不新增解析器结构，三个现有源分别注册到 qresolve::Resolver。

| 现有资源 | 持有者 | 当前用途 |
| --- | --- | --- |
| SystemResolver | 全局 Resolver 注册表 | 解析普通 DNS 名称和 IP，为 H3 DDNS 的普通域名 origin 提供引导 |
| 匿名 H3Resolver | 全局 Resolver 注册表 | 查询 DHTTP E 记录，使用自身现有缓存，不借用某个 Server 的身份 |
| MdnsResolverSet | 全局 Resolver 注册表与 run 共享引用 | 查询使用实际 mDNS 网卡资源；run 维护同一集合并安装本地应答 |

H3 与 mDNS 的协议资源和生命周期分别成立，无需包装成一个结构。全局 Resolver 已负责并发查询和结果流合并，Pishoo 不再重复实现聚合。run 只额外持有需要维护的 MdnsResolverSet 克隆。

### Pishoo Server

保留原有 profile、endpoint、config、access、workspace、chat、router、sandbox、exec_tasks。新增一个实际协议资源成员：

```rust
publisher: Option<Arc<ddns::H3Resolver>>,
```

- 只有外网监听身份构造 H3 发布器；其他身份没有这项资源。
- 发布器固定绑定本 Server 已加载的 Endpoint；进行中的发布 future 克隆它。
- 不额外保存 publish_enabled、last_published、failed 或 next_retry。是否需要该资源从 listen 推导；Option 表示实际资源是否存在。
- 关闭时先由 run 调用 DNS 撤回，再由 Server.close 丢弃 publisher。
- 重载 Lib、代理和 Router 保留它；证书链切换本版要求重启，不能仅把磁盘新凭据塞进旧发布器。

ServerConfig 不新增字段，数据库 schema 不变。沿用 ddns 的编译期默认 origin；第一版不增加实例配置文件、DNS 开关或服务端列表。

### ddns H3Resolver

沿用现有类型及 cache，将弱引用改为直接共享已加载的 Endpoint：

```rust
pub struct H3Resolver {
    server_origin: url::Url,
    endpoint: Option<dhttp::Endpoint>,
    cache: cache::LookupCache,
}
```

| 成员 | 完整含义 |
| --- | --- |
| server_origin | 已校验 HTTPS origin；不能在 dhttp.net 命名空间内；无用户信息、额外路径、query、fragment |
| endpoint | None 表示匿名查询；Some 表示具名查询及发布，持实际 Endpoint 资源的克隆 |
| cache | 已有查询缓存；由服务端缓存约束和 DNS TTL 决定有效期 |

原结构名、匿名能力和 query/publish 组合不变。没有新增 signer 字段；签名按需从 Endpoint 已有内存材料导出 LocalAuthority。H3LookupError 与 H3PublishError 中的 EndpointGone 变体均随弱引用删除；IdentityRequired 等其他原有错误保留。

### dhttp Endpoint 与 Network

Endpoint 的现行真实成员已由本次批准确认：

```rust
pub struct Endpoint {
    pub(crate) quic: Arc<qconn::QuicEndpoint>,
}

pub type ListenFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
```

新增 `Endpoint::local_authority(&self) -> dhttp::Result<qtls::LocalAuthority>`，从 quic.identity 的名称、证书、signing_key 和 OCSP 构造现成 LocalAuthority，不读磁盘、不访问 Network、不增加成员。

DhttpNetwork 的 listeners 与 pool 保持不变。listen 的登记阶段返回拥有原清理 guard 的 ListenFuture。清理仍只撤销服务与名称登记，保留连接池、socket 和已有请求；不新增 stop_listening 或 shutdown。

ListenFuture 是标准 future 的类型别名，不是新状态结构。其唯一作用是持有已登记监听的实际生命周期；不增加 ready channel、is_listening 标志或旁路通知。

### 复用的底层结构

- qtls::LocalAuthority、qbase::endpoint::Endpoint、qconn::QuicEndpoint：成员不变。
- AddressBook：成员不变；增加只读 inner_bindings 方法，派生当前内网绑定和既有网卡元数据。
- AddressEvent：Added、Removed、BoundRemoved 及载荷不变；Pishoo 仅把事件当成重新核对 mDNS 资源的触发。
- MdnsBinding、MdnsResolverSet、MdnsResolver：成员不变；mDNS 记录更新复用现有 publish_endpoints/remove_name。
- H3 查询缓存、DDNS 存储记录与 AppState：成员不变。
- h3x：结构、成员、接口和行为均不变。

## 文件组织与依赖装配

- 新增普通 `pishoo/src/dns.rs`，在 `pishoo.rs` 使用 `mod dns;`，不使用 include、mod.rs 或实现片段。
- 保留 server.rs 对运行循环、Server.load/reload/close 的所有权；dns.rs 只实现本文六个函数，不定义自有解析器结构。
- Pishoo 增加本地 dyns 包依赖，Rust crate 名使用 ddns，开启 h3、mdns；增加 qprotocol 以读取 Dock 的实际资源。qresolve 通过 `use dhttp::resolve as qresolve` 使用现成重导出。
- workspace 根级补齐 crates.io patch，把 ddns 的 dhttp、dhttp-home、qtls、qresolve、qbase、qprotocol 等实际交叉依赖统一到同一组本地版本。依赖子仓的 patch 不会替代顶层 workspace 的依赖选择。
- 实施时用 cargo tree 检查这些包是否存在同版本不同来源的重复实例。尤其 qresolve 注册表、AddressBook、Dock 和 qtls 信任资源必须来自相同 crate 实例，否则会得到互不相通的全局资源。

这些只是依赖与文件装配，不新增公开 DNS 操作路由；业务请求仍走已有 Endpoint 和 Server Router。

## 全部新增或修改的签名

以下为本次批准并冻结的具体接口差异。

### Pishoo

```rust
// 跨模块函数，全部 crate 内部
fn install() -> io::Result<ddns::mdns::MdnsResolverSet>;
fn authority(endpoint: &dhttp::Endpoint) -> io::Result<qtls::LocalAuthority>;
fn publisher(endpoint: &dhttp::Endpoint) -> io::Result<Arc<ddns::H3Resolver>>;
async fn sync_mdns(mdns: &ddns::mdns::MdnsResolverSet,
                   endpoints: &[dhttp::Endpoint],
                   removed_bounds: &[SocketAddr]) -> io::Result<()>;
async fn publish(name: String, publisher: Arc<ddns::H3Resolver>,
                 addresses: Arc<[qresolve::EndpointAddr]>) -> Option<tokio::time::Instant>;
async fn withdraw(endpoint: &dhttp::Endpoint,
                  publisher: Option<&ddns::H3Resolver>,
                  mdns: &ddns::mdns::MdnsResolverSet) -> io::Result<()>;

// Server.listen 修改；其余方法签名保持
async fn Server::listen(&self) -> Result<dhttp::ListenFuture>;
```

sync_mdns 的 endpoints 只包含已经监听、内网范围开启且应用尚未关闭的身份，是本轮从 Server 集合派生的短生命周期参数，不是持久身份表。removed_bounds 是本轮消费的 BoundRemoved 事件值，返回后丢弃。publish 的 name 来自当前 Server，只有当前请求 future 持有它；返回值是本请求决定的下一次维护时刻，空集合撤回成功不需要续期，所以返回 None。

run 内部创建批、排空批、选出下次时刻的闭包属于局部算法，不导出为另一层管理 API。

### dhttp 与 dquic

```rust
// 新增
pub fn Endpoint::local_authority(&self) -> dhttp::Result<qtls::LocalAuthority>;
pub type ListenFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

// 修改；S、B 的现有 trait bounds 保持
pub async fn Endpoint::listen<S, B>(&self, scopes: Scopes, service: S)
    -> dhttp::Result<ListenFuture>;

pub(crate) async fn DhttpNetwork::listen(&'static self,
    endpoint: &Endpoint, scopes: Scopes, service: BoxService)
    -> dhttp::Result<ListenFuture>;

// qprotocol AddressBook 新增只读方法
pub fn inner_bindings(&self) -> Vec<(SocketAddr, qudp::BoundDevice)>;
```

inner_bindings 返回的是派生的瞬时值，不增加匿名持久状态容器。只返回当前有 Internal 地址、有效端口且带网卡元数据的绑定；不含 Loopback。调用方仍用 Dock.find_socket 判断 socket 当前存在。

### ddns 与 DDNS 服务端

```rust
// H3Resolver 构造修改
pub fn H3Resolver::new(server_origin: url::Url, endpoint: &dhttp::Endpoint)
    -> Result<Self, InvalidServerOrigin>;

// 返回实际租期；空集合撤回成功返回 Duration::ZERO
pub async fn H3Resolver::publish_endpoints(&self, name: &str,
    endpoints: impl IntoIterator<Item = qresolve::EndpointAddr>)
    -> Result<std::time::Duration, H3PublishError>;

// H3PublishError 新增无载荷变体
InvalidLease,
```

H3Resolver::anonymous、lookup、clear_cache 以及 qresolve::Publish trait 的签名不变。Publish trait 实现把成功返回的 Duration 映射成 ()；Pishoo 直接使用 publish_endpoints 取得租期。

MdnsResolverSet 的 Resolve::lookup 签名不变，但实现改为按网卡流式交付，不等待所有网卡查询完成。其候选分组查询接口保持原语义。

服务端 publish、lookup 的 HTTP handler 签名不变，只修改响应头。ddns 的现有 freshness 局部 helper 增加 no-store/no-cache 处理，不新增缓存结构。

## 名称解析函数与伪代码

### install

```text
install():
    origin = 解析 ddns::resolvers::DHTTP_NAME_SERVICE
    h3 = H3Resolver::anonymous(origin)      // origin 校验失败立即返回
    mdns = MdnsResolverSet::new(DHTTP_MDNS_SERVICE_DOMAIN)
    Resolver::add(Arc(SystemResolver))
    Resolver::add(Arc(h3))
    Resolver::add(Arc(mdns.clone()))
    return mdns
```

注册发生在任何出站操作及 Network.init 之前。保留其他应用已经注册的源，不清空全局注册表。以下名称筛选是本稿要求的现有方法体调整，不代表当前实现已经具备，也不增加状态或包装结构。

run 是进程级入口，本版只调用一次；不以新增全局 bool 实现可重入启动。嵌入式反复 run 和解析器卸载不在本版范围。

### 各解析源的名称筛选

用各模块的无状态局部 helper 从 hostname 识别 DNS host，分类时做小写及末尾点规范化；保留完整输入继续查询。IPv6、普通端口与 DHTTP 证书序号仍由对应源按原规则校验。不增加跨模块分类类型或配置字段。

```text
SystemResolver.lookup(hostname, servname, family):
    识别出的 DNS host 属于 dhttp.net 命名空间 -> 返回 NotFound
    否则执行现有 getaddrinfo 流程，保留普通端口与 IP 语义

H3Resolver.lookup(hostname, servname, family):
    识别出的 DNS host 不属于 dhttp.net 命名空间 -> 返回 NoRecordFound
    否则执行现有 E 记录查询与证书序号处理
    保留 origin 防递归检查

MdnsResolverSet.lookup(hostname, servname, family):
    不支持的名称 -> 返回 NotFound
    支持的 DHTTP 名称及既有 local 名称 -> 查询现有匹配网卡实例
```

SystemResolver 跳过 DHTTP 名称是本提案对 qresolve 的具体方法体变化，避免它把 DHTTP 证书序号当成 TCP/UDP 端口；不增加新结构或修改其签名。H3 名称筛选只约束标准 Resolve 查询入口，已有候选分组接口不在此处扩大或收缩支持范围。

### qresolve Resolver 的现有聚合

```text
Resolver.lookup(hostname, servname, family):
    对已注册的每个源并行调用 lookup
    第一个源交付成功结果流后，返回与其他迟到结果流的合并流
    失败源跳过；所有 lookup 均失败时返回现有错误
    合并流 Drop 时丢弃其中持有的未完成查询
```

复用当前行为，不在 Pishoo 加第二层聚合：成功的空流算成功，全部无候选最终以流 EOF 表达；只有所有 lookup 都失败才在初始调用返回错误。当前聚合器不去重，路径候选的去重沿用底层连接发现流程，不在 Pishoo 维护 seen 集合。

既有 RecordStream 的元素没有错误分支。开始交付后某源失败不能再返回 lookup 错误，不新增旁路通知或错误通道。H3 慢不阻断 mDNS，迟到候选继续交给底层。

### MdnsResolverSet.lookup

```text
lookup(name, servname, family):
    instances = snapshot() 中 family 匹配的实例
    并行启动每个实例的现有 lookup
    在现有 lookup 方法体内用 FuturesUnordered 合并各网卡结果流
    首个成功流交付后继续推进其余查询，不等待所有网卡完成
    保留每个实例实际的 Source::Mdns { nic, family }
    不把一个网卡的 source 贴到另一个网卡的候选地址上
```

改变的是标准 Resolve 流式适配实现，不更改 CertificateChainKey 候选分组接口；不混合或重新定义设备/证书链身份。

## 内存凭据与发布器函数

### Endpoint.local_authority

```text
local_authority():
    identity = self.quic.identity
    return LocalAuthority::from_signing_key(
        identity.name(), identity.cert_chain().to_vec(),
        identity.signing_key().clone(), identity.ocsp().to_vec()
    )
```

沿用 qtls 校验与错误映射。不缓存返回值；底层 signing_key 的 Arc 仍共享同一实际资源。删除磁盘身份目录不影响已经加载的材料，证书过期或吊销仍可能使服务端拒绝撤回。

### authority

```text
authority(endpoint):
    a = endpoint.local_authority()
    ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(a.certificates())
    要求 ski.chain().usage() == CertificateUsage::ClientAndServer
    否则返回 InvalidInput，说明该凭据不能发布非空 E 记录
    return a
```

这是 Pishoo 启动时的发布能力校验；没有新身份结构。只对 listen 非零身份执行，listen 为零的客户端身份不因无发布能力被拒绝。

### publisher 与 H3Resolver.new

```text
publisher(endpoint):
    校验 authority(endpoint)
    origin = 解析编译期默认 origin
    return Arc(H3Resolver::new(origin, endpoint))

H3Resolver::new(origin, endpoint):
    校验 origin
    return H3Resolver {
        server_origin: origin,
        endpoint: Some(endpoint.clone()),
        cache: 现有默认 LookupCache
    }
```

mDNS 单独开启的身份只校验 authority，不创建用不到的 H3 发布器。H3Resolver 的匿名构造保持 endpoint=None。

### Server 的加载重载与关闭

```text
Server.load(profile, runtime):
    按既有流程加载 config 和 Endpoint
    if config.listen != 0: authority(endpoint)?
    publisher = if config.listen 属于 2 或 3:
                    Some(dns::publisher(endpoint)?)
                else: None
    按既有流程创建 Access、Sandbox、Workspace、Chat 与 Router
    return Server { 原有成员, publisher }

Server.reload():
    按既有规则拒绝 listen/exec 变化
    从 profile 读取证书链，与 endpoint.local_authority().certificates 比较
    证书链变化 -> 返回 InvalidConfig，说明凭据更换需重启
    按既有流程加载 Lib 并替换 Router/config
    保留 endpoint 与 publisher

Server.close():
    按既有流程清除 Router，关闭并等待应用任务
    进入关闭收尾时 take publisher，不能因应用等待超时跳过资源释放
```

磁盘私钥或 OCSP 的更新也在下次启动时生效；不轮询身份文件。证书链比较只在明确 SIGHUP 重载时执行。run 识别关闭身份仍沿用既有 exec_tasks.is_closed，不增加 active/registered 成员。

## 监听登记函数与伪代码

### Endpoint.listen 与 DhttpNetwork.listen

```text
Endpoint.listen(scopes, service):
    用现有 Tower 适配转成 BoxService
    return Network.global().listen(self, scopes, service).await

Network.listen(endpoint, scopes, service):
    在 listeners 锁内检查名称冲突
    完成 qconn 名称登记及原有接入回调装配
    listeners.insert(name, service)
    创建现成 scopeguard，清理 listeners 和 ServerRegistry 的该名称
    // 登记与 guard 创建之间没有 await 或可失败操作
    在函数返回前把 guard 移入新 async future
    return Box::pin(async move {
        持有 guard
        pending::<()>().await
    })
```

必须先创建 guard 再构造 async move。若 guard 在 future 首次 poll 时才创建，调用方直接丢弃未 poll 的 future 就会泄漏监听登记。错误分支沿现有登记回滚处理；锁内不 await、不调用应用 Service。

全部调用方从 `spawn(endpoint.listen(...))` 调整为：

```text
listener = endpoint.listen(scopes, service).await?  // 此时登记已完成
spawn(async move { listener.await })              // 持有监听生命周期
```

这改变 await 的交付语义，需要同步检查测试、示例和调用者。Pishoo 仍不保存 listener JoinHandle，不恢复监听任务集合，也不允许删身份后同名重新登记。

### Server.listen

```text
listen():
    要求 config.listen != 0；调用方仅对需监听身份调用
    从启动 config 派生 Scopes
    克隆 endpoint、router，创建现有 Tower Service
    await endpoint.listen(scopes, service)
    将已登记的 ListenFuture 返回给 run
```

删除原 listen=0 返回成功的空分支；该内部调用误用返回现有 InvalidConfig。URI 与握手身份核对继续归 dhttp。

## 地址快照与 mDNS 维护函数

### AddressBook.inner_bindings

```text
inner_bindings():
    锁住现有 state
    从 state.inner 的实际 endpoint -> bound 映射取得 bound
    仅选择 endpoint.scope == Internal、端口非零的记录
    从 state.interfaces[bound] 取得 Some(BoundDevice)
    去重并输出 (bound, device.clone())
    释放锁；不访问 socket，不新增 state 成员
```

多个地址别名指向同一 bound 只返回一次。mDNS 端口是 mDNS 实例自己的资源，公布的 QUIC 端口仍来自 mdns_endpoints(bound)。

### sync_mdns

```text
sync_mdns(mdns, endpoints, removed_bounds):
    for instance in mdns.snapshot():
        若 instance.bound_ip 与 removed_bounds 中任一 IP 相同：
            await mdns.remove(由 instance 派生的 MdnsBinding)
    本轮临时 desired: HashMap<MdnsBinding, Vec<EndpointAddr>>
    for (bound, device) in AddressBook.global().inner_bindings():
        socket = Dock.global().find_socket(bound)
        socket 不存在 -> 忽略
        核对 socket.local_addr、bound_device 与快照匹配
        binding = MdnsBinding::new(device.name(), bound.ip())
        desired[binding] 合并 AddressBook.mdns_endpoints(bound)
    每组去重，去掉非 Internal 或无有效端口地址

    for instance in mdns.snapshot():
        从 instance.bound_device/bound_ip 派生 binding
        desired 没有它 -> await mdns.remove(binding)

    for (binding, addresses) in desired:
        instance = await mdns.upsert(binding)
        for endpoint in endpoints:
            instance.publish_endpoints(authority(endpoint), endpoint.name(), addresses)

    处理全部绑定后返回首个错误；其余错误逐项记录
```

desired 仅用于本次对照和地址分组，函数返回后丢弃；实际资源只在既有 MdnsResolverSet 中。多个 QUIC 绑定共享网卡/IP 时，在一个 mDNS 实例中公布端口并集，避免后一次写入覆盖前一个端口。网卡元数据变化在下轮重新核对；不根据 IP 猜网卡。

run 使用 `AddressBook.subscribe_punch(Scope::Internal)` 的现有重放与事件作为维护触发。BoundRemoved 即使没有地址也会触发核对。接收连续事件后用 try_recv 排空本轮已到达事件，把 BoundRemoved.bound 临时交给 sync_mdns，再取完整快照；不重放每个旧事件的地址值。

删除绑定后即使系统迅速复用了同一个网卡名和 IP，也先移除这个 IP 对应的旧 mDNS 实例，再按当前元数据重新 upsert。MdnsBinding 只有网卡名和 IP，单纯对照相同键无法识别网卡重建。这个保守重建可能同时重建其他网卡上同 IP 的实例，但不会撤回其当前有效 QUIC 地址，也不增加硬件标识镜像。

mDNS 查询资源不依赖本地身份是否监听，故即使 endpoints 为空，也维护可用内网实例。失败绑定在维护重试时重新 upsert，不改变 Network 的绑定策略。

本版复用同步 publish_endpoints 与 remove_name：发布是安装查询应答记录，删除是撤销本机应答。没有主动 announcement/goodbye 的保证。远端缓存可能保留到 E 记录的现有 TTL，不能把本机删除成功写成全网立即撤回。

## H3 发布有效期与缓存协议

### 发布响应

本次扩展既有 `/api/v2/publish`，不新建 API 路径或响应对象：

```text
非空发布成功：
    HTTP 200
    DHTTP-DNS-Lease-Millis: <正整数，实际存储租期毫秒数>
    Cache-Control: no-store
    Body: OK

空集合撤回成功：
    HTTP 200
    DHTTP-DNS-Lease-Millis: 0
    Cache-Control: no-store
    Body: OK
```

该头是本次批准的项目协议扩展，不是标准 DNS/HTTP 响应头。服务端使用与存储代码相同的毫秒转换，不能返回四舍五入后更长的租期。头缺失、重复、溢出或不合语义时客户端返回 InvalidLease，不猜测 30 秒。

Pishoo 的固定运行值为 PUBLISH_TIMEOUT=3 秒、MAINTENANCE_RETRY=5 秒、MIN_PUBLISH_LEASE=30 秒，均为 dns 模块局部常量，不新增配置结构。

初版 Pishoo 仅接受非空发布租期至少 30 秒：发布操作期限 3 秒，按请求开始时间加租期的三分之一续期，至少留出多次失败重试空间。服务端已有短租期配置不强制改写；不满足条件时 Pishoo 记录“不支持该租期”并进入维护重试，不能声称已建立稳定续期。这个限制是本提案的固定运行约束，不新增用户配置。

### H3Resolver.publish_endpoints

```text
publish_endpoints(name, addresses):
    立即校验规范名称
    endpoint = self.endpoint.as_ref().ok_or(IdentityRequired)
    要求 endpoint.name == name
    authority = endpoint.local_authority()      // 不读磁盘
    用既有 packet 算法编码完整地址集合及主证书链序号
    非空地址要求 primary chain；空集合仍可走既有撤回语义
    用既有 SignatureFields 签名
    使用原具名 endpoint.post(uri).header(...).write(packet).await
    await RequestWriter.shutdown()
    await response，检查 200；失败沿原有有界错误 body 返回
    读取并验证 DHTTP-DNS-Lease-Millis
    非空集合必须 lease > 0；空集合必须 lease == 0
    清除本 resolver 的该名称查询缓存
    return Duration::from_millis(lease)
```

超时属于 Pishoo 发布操作，不恢复 dhttp 的流读写期限。具名失败不回退匿名、HTTP/1 或其他发布传输。ddns 原有 identity_profile/local_authority 局部文件读取 helper 在无其他调用时删除，不保留备用读盘分支。

### H3 查询与 trait 适配

```text
H3Resolver.lookup / lookup_candidates：
    标准 Resolve 查询入口先筛选 DHTTP 命名空间
    保持原名称与证书序号校验、origin 防递归、分组与 family 过滤
    endpoint = self.endpoint.as_ref()         // 删除 Weak.upgrade 与 EndpointGone 分支
    命中仍有效的既有缓存 -> 返回原候选
    否则按原流程通过匿名或具名 Endpoint 查询
    计算 freshness；零值不写入 LookupCache
    返回带 Source::H3 的原候选记录流

impl Publish for H3Resolver 的 publish：
    同步收集 trait 传入的地址迭代器
    await self.publish_endpoints(name, addresses)
    成功租期映射为 ()，错误保留具体 source 并转为 io::Error

impl Resolve for H3Resolver 的 lookup：
    继续委托 H3Resolver.lookup，错误映射与现有签名不变

freshness(dns_ttl, headers)：
    若 Cache-Control 任一字段含 no-store 或 no-cache -> Duration::ZERO
    否则使用原 min(DNS TTL, HTTP max-age) 并扣除 Age 的规则
```

不要求第三方缓存接受本文自定义租期头；发布租期和查询缓存有效期是分别处理的协议事实。

### 服务端 publish 与 lookup

```text
publish(request):
    保持原鉴权、签名校验和 record_key 提取
    Clear -> storage.clear(name, record_key)，成功后返回 lease=0
    Records -> storage.publish(..., state.ttl)，成功后返回转换后的 state.ttl
    失败不返回成功租期

lookup(request):
    保持原查询、链选择和响应编码
    成功响应添加 Cache-Control: no-store
```

第一版查询采用 no-store，因为现有返回值没有动态记录的剩余存储租期；不修改已签名 DNS 字节来伪造较短 TTL。ddns 的 freshness 必须识别 no-store/no-cache 并返回零，使既有 LookupCache 不保存此响应。服务端 30 秒租期与 DNS 包内 300 秒 TTL 的差异不能被隐藏。

此方案牺牲动态 DDNS 的查询缓存命中率，换取明确有效期。后续若要缓存，必须由服务端提供所有返回记录的最短剩余租期，并与 DNS TTL、Age 取最小值；本版不为此新增存储接口或字段。其他遵守现有 TTL/HTTP 缓存约束的响应仍使用已有缓存。

### Pishoo publish

```text
publish(name, publisher, addresses):
    started = Instant::now()
    result = timeout(3秒, publisher.publish_endpoints(name, addresses.iter().copied()))
    result 成功且 addresses 为空 -> return None
    result 成功且 lease >= 30秒 -> return Some(started + lease / 3)
    result 失败、超时或 lease 太短 -> 记录身份与具体错误
                                     return Some(Instant::now() + 5秒)
```

name 取自任务创建时的 Server.name，不给 H3Resolver 增加名字 getter。一次维护重试的固定 5 秒延迟用于抑制故障下的紧循环；本版不增加逐身份指数退避状态。地址变化仍能触发提前更新。

## 运行入口和维护调度伪代码

### run 的局部资源

| 局部资源 | 用途 |
| --- | --- |
| mdns: MdnsResolverSet | 维护全进程 mDNS 实例，查询器持其共享引用 |
| inner_events: mpsc::UnboundedReceiver<AddressEvent> | 现有内网地址重放与变化触发 |
| outer: watch::Receiver<Arc<[EndpointAddr]>> | 现有外网完整快照订阅 |
| jobs: FuturesUnordered<BoxFuture<'static, Option<Instant>>> | 本轮实际发布 future；每身份最多一个 |
| next_due: Option<Instant> | 当前批返回的最早续期或重试时刻 |
| timer: Option<Pin<Box<Sleep>>> | 有续期或维护重试需求时存在的真实定时器 |

这些是 run 的局部变量及标准 async 资源，不封装进新 State，也不建立第二份 Server 集合。next_due 与 timer 的使用分阶段：批运行中只折叠 next_due，批结束后 move 成 timer，清空 next_due；不会同时把两个值当作独立调度事实。

### 启动

```text
run():
    home、信号与 WasmRuntime 按现有方式准备
    mdns = dns::install()
    订阅 inner_events 与 outer              // 在 Network.init 前订阅，保留初始变化
    串行 Server.load，构造原 Server 集合
    await DhttpNetwork::init()
    for 每个 config.listen != 0 的 Server:
        listener = await server.listen()     // 失败则进入 DNS/应用收尾
        spawn(async move { listener.await })
    创建第一次维护批，removed_bounds 为空
    进入下述 select 循环
```

Server.load 在创建完整 Server 前，对监听身份校验发布 authority；外网身份创建 publisher。不可用网络导致初次远程发布失败不终止服务，身份材料或默认 origin 无效则结束启动。

启动及运行主体放在一个局部 async 块中取得 Result，再统一执行下文退出收尾。Network.init 或监听登记的错误也经过这个收尾路径，不使用会跳过 DNS 清理的外层直接 `?`。新身份只有在 load 与所需监听登记均成功后才插入运行集合。

### 创建一批

```text
创建批（仅 jobs 为空时）：
    丢弃上轮 timer，取本轮当前内网身份；执行 sync_mdns(mdns, endpoints, removed_bounds)
    next_due = 本轮 sync_mdns 失败时的 now+5秒，成功时 None
    addresses = outer.borrow_and_update().clone()
    for 现有 Server 中应用仍开启且 publisher=Some 的身份:
        克隆 publisher 与 server.name，捕获同一个 addresses 快照
        jobs.push(Box::pin(dns::publish(...)))
    没有 jobs -> 直接按 next_due 安装/清除 timer
```

外网快照为空也发送空集合，以清除之前可能存在的记录；撤回成功不周期发送空集合，后续新地址变化会重新触发。每次重读最新快照，既有 watch 版本机制承担更新记录，不新增 dirty 标志。

### 事件循环

```text
loop:
    select:
        Ctrl-C 或 SIGTERM:
            排空当前有限期限发布批
            break，进入退出收尾

        SIGHUP:
            排空当前发布批
            串行扫描身份
            删除身份 -> await withdraw，再 await server.close
            现有身份 -> 按现有约定 reload，保留旧 Endpoint/publisher
            新身份 -> load，成功监听登记后 spawn listener
            按最终有效 Server 集合创建维护批，removed_bounds 为空

        job 结果（jobs 非空时）:
            next_due = 所有 Some(Instant) 与 mDNS 重试时刻的最小值
            批完成 -> next_due move 成 timer，next_due 清空

        内网事件（jobs 为空时）:
            排空本轮已经到达的事件并收集 BoundRemoved.bound
            用这些 removed_bounds 创建维护批

        outer.changed（jobs 为空时）:
            创建维护批，removed_bounds 为空；读取并标记最新 watch 值

        timer 到期（jobs 为空时且 timer 存在）:
            清除 timer，创建维护批，removed_bounds 为空
```

批进行时不消耗内外地址事件，消息队列与 watch 自然保留待处理更新。这样不在请求进行中重复发布旧/新集合，也不保存待发布快照。全进程按最短租期续期，允许较长租期身份一起提前续期；本版不为减少这部分请求增加逐身份调度表。

排空批不是额外总停机期限：所有已启动请求各自受 3 秒期限约束，FuturesUnordered 必须并行 poll 剩余 future，不能逐个重新创建 timeout。mDNS upsert/remove 使用现成短操作；不得把未经期限约束的远程 I/O 放在 select 之外。

SIGHUP 的部分失败沿既有串行重载语义处理。无论扫描或某身份加载是否失败，都对最终仍有效的 Server 集合恢复 DNS 维护；不得因日志分支遗漏而永久停止旧身份续期。

## 删除和退出函数

### withdraw

```text
withdraw(endpoint, publisher, mdns):
    for instance in mdns.snapshot():
        instance.remove_name(endpoint.name())
    if publisher 存在:
        timeout(3秒, publisher.publish_endpoints(endpoint.name(), 空集合))
        失败则返回有身份上下文的 io::Error
    return Ok
```

调用前必须排空本进程该批发布请求。撤回不读取 profile 文件，删除身份目录后仍可尝试；mDNS 本地撤销与 H3 撤回互不阻断。H3 请求失败不阻止清除 Router、关闭 guest/exec 的现有任务资源或处理其他身份。

已经上传到服务端但客户端超时的旧请求仍可能晚于撤回完成，现协议没有请求版本或服务端写入栅栏。本版不承诺严格“撤回后永不再出现”；残余记录由租期到期清理。新增代际协议需要另行设计批准，不通过隐藏计数解决。

### Server.close 与进程退出

```text
删身份：
    排空当前批
    await withdraw(server.endpoint, server.publisher, mdns)，失败记录
    await server.close()                     // 即使撤回失败也执行
    server.close 内丢弃 publisher
    保留现有关闭后的 Server 项，以维持同名恢复需重启约定

退出：
    不创建后续维护批
    排空当前批
    并行尝试全部活跃身份的 withdraw，每项自己的 3秒期限
    逐个执行现有 Server.close，收集原有关闭错误
    await mdns.shutdown()                    // 关闭 Pishoo 拥有的 mDNS 资源
    DNS 错误映射现有 Error::Io，应用收尾仍全部执行
    返回原 run 结果与收尾错误
```

退出不调用 Network.shutdown、Dock.shutdown 或 Endpoint.close。全局解析器注册表与 dhttp 传输仍按原进程生命周期结束。mDNS 的 shutdown 是 Pishoo 自己拥有的协议资源收尾，不扩大为传输关闭。

## 错误与兼容行为

| 场景 | 行为 |
| --- | --- |
| 默认 origin、身份材料、发布权限不合法 | 启动/新增身份加载立即返回现有 InvalidConfig 或 Io，不创建待报错 builder |
| 监听登记失败 | 不发布该身份；启动路径结束并收尾已创建 DNS/应用资源 |
| DDNS 服务暂不可达 | 保持应用服务；记录具体错误；5 秒后维护重试 |
| 一个解析源失败 | 继续其他源；全部无候选时返回 NotFound |
| 一个 mDNS 网卡绑定失败 | 处理其他网卡；5 秒后重新核对实际绑定 |
| 无外网地址 | H3 清除自身记录；等待地址变化，不伪造公网地址 |
| 无内网地址 | 移除无效 mDNS 实例；查询仍可走 H3 |
| 服务端缺少租期头 | InvalidLease；需要先部署协议扩展，不使用固定 TTL 回退 |
| 身份目录已删除 | 使用内存 Endpoint 凭据撤回；凭据被吊销时可能失败，按租期清理 |
| 本机 mDNS 名称删除 | 不再回答该名称；已有远端缓存按原 TTL 过期 |
| SIGHUP 发现证书替换 | Endpoint/发布器保持原凭据；提示需重启，不混用新旧链 |

需要先部署服务端租期响应与查询 no-store，再接入改造后的 ddns 和 Pishoo；旧客户端可忽略新发布响应头。新客户端对旧服务端的发布将报告协议缺口。既有 qresolve::Publish 的调用代码无需消费租期。

## 逐项验收

1. 三个现有解析源分别注册；普通名称由 System 处理，H3 跳过；DHTTP 名称由 H3/mDNS 处理，System 跳过；DDNS origin 不递归，普通端口与证书序号各按正确语义处理。
2. H3 慢、mDNS 快及相反情况都能先交付候选；一个源失败不阻断另一个；空源不阻断其他源结果，全部成功空流以 EOF 结束；丢弃流后不遗留查询任务。
3. listen 失败时零发布；取得未 poll 的 ListenFuture 后直接 Drop 仍撤销登记；取消监听不清理共享连接和 socket。
4. listen 0/1/2/3 发布范围正确；listen 为零仍可出站解析。
5. 初始绑定重放、网卡/IP 新增删除、多端口共网卡、socket 失效全部从实际地址簿派生；Source 保留真实网卡与 family。
6. 非空发布取得租期；超时、缺头、重复头、零值、溢出和短租期均按明确规则处理；续期使用请求开始时间计算，批之间无并发写入同身份。
7. 地址变化发生于发布期间，下一批使用最新完整集合；最后地址消失后清除记录；空集合清除成功不持续发送空请求。
8. 服务端 lookup no-store 被 ddns 遵守，动态记录失效后不仍命中包内 300 秒缓存。
9. 同名两个独立发布者中删除一个，只清除其完整 SKI 对应记录；复制相同凭据的并行进程按明确限制处理。
10. 删除磁盘 profile 后仍能用原内存凭据发送撤回；撤回失败不阻止其他身份与应用关闭。
11. 退出在正在发布、重试、SIGHUP 失败时都不再创建后续批；mDNS 自有任务结束，共享传输不被主动关闭。
12. NAT 验收分开：当前阶段只使用 AddressBook 已有可公布地址，公网直连通过后再验证底层提供的 NAT 映射。接上 DNS 不等于完成 NAT 探测或打洞。

## 已批准的成员与接口变化汇总

| 位置 | 具体变化 | 当前必需用途 |
| --- | --- | --- |
| Pishoo Server | 新增 publisher: Option<Arc<H3Resolver>> | 身份删除后仍能持原凭据撤回，并在进行中请求间共享协议资源 |
| Pishoo dns 模块 | 新增 install、authority、publisher、sync_mdns、publish、withdraw 六个内部跨模块函数 | 装配、校验和维护本轮要求的解析发布能力 |
| Pishoo Server.listen | 改为 async 并返回 Result<ListenFuture> | 登记成功后才首次发布 |
| dhttp Endpoint | 确认现行 quic 成员并替换冻结 name；新增 local_authority | 从原加载材料签名，不重复读取已删除目录 |
| dhttp | 新增 ListenFuture 别名；Endpoint.listen、Network.listen 返回它 | 用实际监听 future 表达登记完成与存续，不加通知状态 |
| qresolve SystemResolver | lookup 方法体跳过 DHTTP 命名空间，签名与成员不变 | 避免把证书序号当成普通传输端口 |
| qprotocol AddressBook | 新增 inner_bindings 只读方法 | 按实际内网绑定建立/移除 mDNS 实例，合并同 IP 的不同 QUIC 端口 |
| ddns H3Resolver | endpoint 从 Weak 改为 Option<Endpoint>；new 参数改为 &Endpoint | 直接持有具名协议资源，不额外保留 Arc 强引用容器 |
| ddns H3Resolver | publish_endpoints 返回 Duration；删除 EndpointGone，增加 InvalidLease | 取得续期依据并报告协议错误 |
| DDNS HTTP 协议 | 发布响应增加 DHTTP-DNS-Lease-Millis；查询响应 no-store | 明确租期，避免签名包 TTL 长于动态记录租期时的陈旧缓存 |

其他变更仅在现有方法体和局部算法内。本文未增加 ServerConfig、Network、AddressBook、qtls、h3x 的成员；不恢复已删除的关闭、配额、WASM 出站或后台编译结构。

## 当前代码定位

- [qresolve 解析及发布 trait](../../dquic/qresolve/src/lib.rs)
- [地址簿与现有订阅](../../dquic/qprotocol/src/addr_book.rs)
- [实际 Endpoint 凭据](../../dhttp/dhttp/src/endpoint.rs)
- [监听登记与 guard](../../dhttp/dhttp/src/network.rs)
- [H3Resolver 资源及构造](../../ddns/src/h3.rs)
- [H3 发布请求](../../ddns/src/h3/publish.rs)
- [mDNS 实例集合](../../ddns/src/mdns.rs)
- [mDNS 应答与本地删除](../../ddns/src/mdns/service.rs)
- [E 记录固定 TTL](../../ddns/src/core/parser/packet.rs)
- [DDNS 服务端 handler](../../ddns-sever/src/router.rs)
- [Server 启动重载与关闭](../pishoo/src/server.rs)

## 2026-10-03 线上旧版临时兼容

用户明确要求先跳过尚未上线的缺失租期头校验。publish_endpoints 签名保持不变：HTTP 200 缺 DHTTP-DNS-Lease-Millis 时，非空发布暂返回300秒续期窗口，空发布返回0；这是当前线上查询 TTL 的临时兼容值，不能视为服务端确认的租期。有该头时继续校验原有数值、重复和清空语义。部署完成后撤销此缺头兼容。此决定不放宽发布身份、证书或签名校验。

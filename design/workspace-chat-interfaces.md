# Workspace 与 Chat 接入清单

本文件补充 [Pishoo 清单](pishoo-interfaces.md)。2026-10-02 用户要求保留远端 `pishoo/feat/daccess@9b733c5` 的工作并先 rebase 适配，审批和联系人以 `daccess/feat/fit-pishoo@cf8f72f` 为准；当时暂缓底层接口适配；2026-10-03用户批准并要求接入生产出站，当前接缝见下文。

## 资源归属与本轮调整

- Server 直接增加 `workspace: Arc<Workspace>`、`chat: Arc<Chat>`。原 `WorkspaceState`、`ChatState` 更名为对应业务名称，保持分支中的业务数据、持久队列和数据库模型。
- 每个 profile 的 `db/workspace.db` 保存资料、联系人投递及能力决定；`db/chat.db` 保存聊天、投递作业和远端授权观察。SQL 与当前 schema 版本沿用目标分支。现有旧库版本或 Syncing 联系人不能假定自动兼容，迁移需单独验证。
- 2026-10-04 用户要求启动初始化：WorkspaceStore/ChatStore 的既有 open/migrate 仅在完全空数据库事务建表与写入当前版本；已有库先校验版本和必要结构，不自动采用无版本业务库或创建缺失业务表。数据库及头像路径不变，新文件/目录使用私有权限。新数据不含示例联系人、能力批准或消息；重启保留资料、队列和消息。无新增类型、字段、方法或跨模块函数。
- `access_router` 改为只接收 AccessService；Server 显式合并 Workspace/Chat Router。`authorize` 签名不变；202、查询归属与公开资料规则见 Pishoo 清单。
- Workspace/Chat 各保留自己的 worker 句柄、唤醒和关闭信号。句柄保证每个资源实例只启动一个 worker；Notify 用于新任务/授权更新后唤醒；关闭信号结束等待。profile_write、contact_write 和 outbound_send 沿用分支的文件/数据库更新与申请操作串行约束，不是请求或传输并发配额。
- 新增两个 `shutdown(&self)` 方法：发出关闭信号，取出并中止现有 worker，等待句柄结束。Server.close 在现有15秒退出等待内调用它们。运行期间使用同一 Workspace/Chat，身份变更统一重启。
- 2026-10-03 用户要求接入生产出站，并批准 [dhttp 清单](dhttp-interfaces.md) 的 Request owner_hash 发送前校验与 RemoteIdentityChanged 错误。Workspace/Chat 的 OutboundTransport 均直接由现有 `dhttp::Endpoint` 实现，由 Server.load 用已有 Endpoint 的 clone 装配，不新增生产结构或成员。Workspace 从内存 LocalAuthority 派生发送者身份，从响应中的已验证 RemoteAuthority 提取远端身份；Chat 将已有联系人 SubjectId 解析为 OwnerHash，在实际连接开流前校验。出站保留15秒总期限和1MiB响应上限；请求使用 WndBuf/RequestWriter，发送完显式 shutdown。旧网络测试素材仍保留于 deferred，新验收使用现行 QUIC 接缝。
- `/std/message` 继续要求 daccess 允许和分支既有的能力决定校验；`/std/workspace-api/*`、`/std/chat-api/*` 的本地管理核对 owner。能力请求与普通访问审批分开保存，Pishoo 不另建 daccess 审批状态。

## 保留的结构与接口

以下为本次接入的字段、枚举、trait、方法及跨模块函数清单；省略 use 和方法体，类型名称按对应源文件解析。模块内部无状态 helper 不冻结。测试 fixture 不进入生产接口清单。


### `pishoo/src/workspace.rs`

```rust
pub struct Workspace {
    owner: Owner,
    profile: String,
    store: WorkspaceStore,
    access: Arc<AccessService>,
    outbound: RwLock<Option<Arc<dyn OutboundTransport>>>,
    chat: RwLock<Option<Weak<crate::chat::Chat>>>,
    outbound_send: tokio::sync::Mutex<()>,
    worker_notify: Arc<Notify>,
    worker_shutdown: CancellationToken,
    worker_handle: StdMutex<Option<JoinHandle<()>>>,
    profile_write: tokio::sync::Mutex<()>,
    contact_write: tokio::sync::Mutex<()>,
}

impl Workspace {
    pub(crate) fn new(
        profile: String,
        owner_name: String,
        owner_subject_id: SubjectId,
        store: WorkspaceStore,
        access: Arc<AccessService>,
    ) -> Self;
    pub(crate) async fn configure_outbound(&self, outbound: Arc<dyn OutboundTransport>);
    fn start_worker(self: &Arc<Self>);
    fn wake_worker(&self);
    pub(crate) async fn shutdown(&self);
    pub(crate) async fn configure_chat(&self, chat: Arc<crate::chat::Chat>);
    pub(crate) async fn update_remote_chat_grant(
        &self,
        contact_name: &str,
        subject_id: &SubjectId,
        granted_access: &access_control::GrantedAccess,
    ) -> Result<(), sea_orm::DbErr>;
}

impl Drop for Workspace {
    fn drop(&mut self);
}

struct RuntimeContext {
    profile: String,
    owner_name: String,
    badges: BadgeCounts,
}

struct BadgeCounts {
    pending_reviews: u64,
    // Counts incoming contacts that carry at least one requested capability.
    incoming_contacts: Option<u64>,
}

pub(crate) fn router(state: Arc<Workspace>) -> Router;
```


### `pishoo/src/workspace/actor.rs`

```rust
pub struct Owner {
    name: String,
    subject_id: SubjectId,
}

impl Owner {
    pub fn new(name: String, subject_id: SubjectId) -> Self;
    pub fn ensure(&self, visitor: Option<&Visitor>) -> Result<(), StatusCode>;
    pub fn name(&self) -> &str;
    pub fn subject_id(&self) -> &SubjectId;
}
```


### `pishoo/src/workspace/approvals.rs`

```rust
enum ApprovalStatus {
    #[default]
    Pending,
    Expired,
}

pub(super) struct ApprovalQuery {
    status: ApprovalStatus,
    page: Option<u64>,
    page_size: Option<u64>,
}

enum ApprovalItem {
    Capability {
        request_id: i64,
        contact_name: String,
        capability_id: &'static str,
        capability_version: &'static str,
        requested_at: i64,
        expired_after: i64,
    },
    Access {
        id: i64,
        visitor: String,
        method: String,
        api: String,
        reason: String,
        requested_at: i64,
        expired_after: i64,
    },
}

impl ApprovalItem {
    fn requested_at(&self) -> i64;
    fn expired_after(&self) -> i64;
}

pub(super) struct ApprovalPage {
    items: Vec<ApprovalItem>,
    total: usize,
    page: u64,
    page_size: u64,
}

pub(super) async fn list(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Query(query): Query<ApprovalQuery>,
) -> Result<Json<ApprovalPage>, ApiError>;

pub(super) async fn delete(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<StatusCode, ApiError>;
```


### `pishoo/src/workspace/capabilities.rs`

```rust
pub(crate) enum CapabilityVisibility {
    Public,
}

pub(crate) enum CapabilityApproval {
    None,
}

pub(crate) struct CapabilityEndpoint {
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
}

pub(crate) struct CapabilityDescriptor {
    pub(crate) id: &'static str,
    pub(crate) version: &'static str,
    pub(crate) visibility: &'static str,
    pub(crate) approval_mode: &'static str,
    pub(crate) selectable: bool,
    pub(crate) endpoints: Vec<CapabilityEndpoint>,
}

pub(crate) enum BuiltInCapability {
    PublicProfile,
}

impl BuiltInCapability {
    pub(crate) const fn id(self) -> &'static str;
    pub(crate) const fn version(self) -> &'static str;
    pub(crate) const fn visibility(self) -> CapabilityVisibility;
    pub(crate) const fn approval(self) -> CapabilityApproval;
    pub(crate) const fn endpoints(self) -> &'static [CapabilityEndpoint];
    pub(crate) fn matches(self, method: &Method, path: &str) -> bool;
    pub(crate) fn public_endpoint(method: &Method, path: &str) -> bool;
}

pub(crate) fn descriptors() -> Vec<CapabilityDescriptor>;

pub(crate) fn requested_access_for(id: &str) -> Option<RequestedAccess>;

pub(crate) fn offered_access_for(id: &str) -> Option<GrantedAccess>;

pub(crate) async fn list(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<access_control::Visitor>>,
) -> Result<Json<Vec<CapabilityDescriptor>>, StatusCode>;
```


### `pishoo/src/workspace/contacts/worker.rs`

```rust
struct Job {
    id: i64,
    target: String,
    application_id: String,
    sender_subject_id: Vec<u8>,
    recipient_subject_id: Option<Vec<u8>>,
    description: String,
    requested_capabilities: Vec<String>,
    offered_capabilities: Vec<String>,
    status: String,
    expired_after: i64,
    attempt_count: i64,
    lease_until: i64,
}

pub(crate) fn spawn(
    state: Weak<Workspace>,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
) -> JoinHandle<()>;
```


### `pishoo/src/workspace/contacts.rs`

```rust
pub(crate) struct NewRequest {
    target_name: String,
    description: String,
    #[serde(default)]
    requested_capabilities: Vec<String>,
    #[serde(default)]
    offered_capabilities: Vec<String>,
}

pub(crate) struct OutboundRequest {
    id: i64,
    target_name: String,
    description: String,
    requested_capabilities: Vec<String>,
    offered_capabilities: Vec<String>,
    status: String,
    expired_after: i64,
    delivery_deadline: i64,
    remote_expired_after: Option<i64>,
    last_checked_at: Option<i64>,
    error_message: Option<String>,
    created_at: i64,
    updated_at: i64,
}

pub(crate) struct PageQuery {
    page: u64,
    page_size: Option<u64>,
}

impl Default for PageQuery {
    fn default() -> Self;
}

pub(crate) struct RequestPage {
    items: Vec<OutboundRequest>,
    total: u64,
    page: u64,
    page_size: u64,
}

pub(crate) struct CapabilityRequest {
    pub(super) request_id: i64,
    pub(super) contact_name: String,
    pub(super) capability_id: &'static str,
    pub(super) capability_version: &'static str,
    pub(super) status: &'static str,
    pub(super) requested_at: i64,
    pub(super) expired_after: i64,
}

pub(crate) struct CapabilityDecisionQuery {
    request_id: i64,
    capability_version: String,
}

pub(crate) struct CapabilityGrantQuery {
    request_id: Option<i64>,
    capability_version: Option<String>,
}

struct RemoteApplication<'a> {
    application_id: &'a str,
    class: &'static str,
    description: &'a str,
    requested_access: &'a RequestedAccess,
    offers: &'a GrantedAccess,
}

struct RemoteStatus {
    application_id: String,
    status: String,
    name: String,
    subject_id: String,
    received_at: i64,
    expired_after: i64,
    #[serde(default)]
    granted_access: GrantedAccess,
}

pub(crate) async fn ensure_chat_access(state: &Workspace, name: &str) -> Result<(), ApiError>;

pub(crate) async fn grant_capability(
    state: &Workspace,
    name: &str,
    capability: &str,
    expected: Option<(i64, &str)>,
) -> Result<(), ApiError>;

pub(crate) async fn revoke_capability(
    state: &Workspace,
    name: &str,
    capability: &str,
) -> Result<(), ApiError>;

pub(crate) async fn deny_capability(
    state: &Workspace,
    name: &str,
    capability: &str,
    expected_request_id: i64,
) -> Result<(), ApiError>;

pub(super) async fn pending_capability_requests(
    state: &Workspace,
) -> Result<Vec<CapabilityRequest>, ApiError>;

pub(super) async fn expired_capability_requests(
    state: &Workspace,
) -> Result<Vec<CapabilityRequest>, ApiError>;

pub(crate) async fn capability_requests(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<Vec<CapabilityRequest>>, ApiError>;

pub(crate) async fn create(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Json(body): Json<NewRequest>,
) -> Result<(StatusCode, Json<OutboundRequest>), ApiError>;

pub(crate) async fn grant(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path((name, capability)): Path<(String, String)>,
    Query(query): Query<CapabilityGrantQuery>,
) -> Result<StatusCode, ApiError>;

pub(crate) async fn revoke(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path((name, capability)): Path<(String, String)>,
) -> Result<StatusCode, ApiError>;

pub(crate) async fn deny(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path((name, capability)): Path<(String, String)>,
    Query(query): Query<CapabilityDecisionQuery>,
) -> Result<StatusCode, ApiError>;

pub(crate) async fn list(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Query(query): Query<PageQuery>,
) -> Result<Json<RequestPage>, ApiError>;

pub(crate) async fn get(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(id): Path<i64>,
) -> Result<Json<OutboundRequest>, ApiError>;

pub(crate) async fn refresh(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(id): Path<i64>,
) -> Result<Json<OutboundRequest>, ApiError>;

pub(crate) async fn delete(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError>;
```


### `pishoo/src/workspace/directory.rs`

```rust
pub(crate) struct DirectoryEntry {
    name: String,
    saved: bool,
    chat_available: bool,
    remote_chat_granted: Option<bool>,
}

pub(crate) async fn list(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<Vec<DirectoryEntry>>, ApiError>;

pub(crate) async fn approved_chat_grant(
    state: &Workspace,
    visitor: &Visitor,
) -> Result<bool, DbErr>;

pub(crate) async fn save(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError>;

pub(crate) async fn unsave(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError>;
```


### `pishoo/src/workspace/outbound.rs`

```rust
pub(crate) struct RemoteResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub remote_subject_id: Vec<u8>,
}

pub(crate) trait OutboundTransport: Send + Sync {
    fn sender_subject_id(&self) -> Result<Option<Vec<u8>>, String>;

    fn request<'a>(
        &'a self,
        target: &'a str,
        method: Method,
        path: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<RemoteResponse, String>>;
}
```


### `pishoo/src/workspace/profile.rs`

```rust
pub(crate) struct PublicProfile {
    display_name: Option<String>,
    avatar_url: Option<String>,
    updated_at: i64,
}

struct RemotePublicProfile {
    display_name: Option<String>,
    avatar_url: Option<String>,
    updated_at: i64,
}

enum AvatarFormat {
    Jpeg,
    Png,
    Webp,
}

impl AvatarFormat {
    fn extension(self) -> &'static str;
    fn media_type(self) -> &'static str;
}

pub(crate) async fn get_public_profile(
    State(state): State<Arc<Workspace>>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<PublicProfile>), ApiError>;

pub(crate) async fn get_public_avatar(
    State(state): State<Arc<Workspace>>,
    headers: HeaderMap,
) -> Result<Response, ApiError>;

pub(crate) async fn get_avatar(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    headers: HeaderMap,
) -> Result<Response, ApiError>;

pub(crate) async fn put_avatar(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ProfileSettings>, ApiError>;

pub(crate) async fn delete_avatar(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<ProfileSettings>, ApiError>;

pub(crate) async fn get_remote_profile(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(name): Path<String>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<PublicProfile>), ApiError>;

pub(crate) async fn get_remote_avatar(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError>;
```


### `pishoo/src/workspace/settings.rs`

```rust
pub(crate) struct ProfileSettings {
    identity_name: String,
    display_name: Option<String>,
    avatar_url: Option<&'static str>,
    updated_at: i64,
}

pub(super) struct StoredProfile {
    pub display_name: Option<String>,
    pub avatar_name: Option<String>,
    pub updated_at: i64,
}

pub struct ProfilePatch {
    /// An empty string clears the local display name.
    display_name: String,
}

pub(super) fn storage_error(error: DbErr) -> ApiError;

pub(super) fn require_owner(state: &Workspace, visitor: Option<&Visitor>) -> Result<(), ApiError>;

pub(super) async fn read_stored_profile(
    db: &impl ConnectionTrait,
) -> Result<StoredProfile, ApiError>;

pub(super) fn profile_settings(identity_name: &str, stored: StoredProfile) -> ProfileSettings;

pub async fn get_profile(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<ProfileSettings>, ApiError>;

pub async fn patch_profile(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Json(body): Json<ProfilePatch>,
) -> Result<Json<ProfileSettings>, ApiError>;
```


### `pishoo/src/workspace/store.rs`

```rust
pub enum StoreError {
    #[snafu(display("failed to create Workspace database directory `{}`", path.display()))]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to open Workspace database `{}`", path.display()))]
    Connect {
        path: PathBuf,
        source: sea_orm::DbErr,
    },
    #[snafu(display("failed to migrate Workspace database `{}`", path.display()))]
    Migration {
        path: PathBuf,
        source: sea_orm::DbErr,
    },
    #[snafu(display("unsupported Workspace database version {version} in `{}`", path.display()))]
    UnsupportedVersion { path: PathBuf, version: i64 },
}

pub struct WorkspaceStore {
    db: DatabaseConnection,
    profile_assets: PathBuf,
}

impl WorkspaceStore {
    pub async fn open(profile: &IdentityProfile) -> Result<Self, StoreError>;
    pub(crate) fn db(&self) -> &DatabaseConnection;
    pub(crate) fn profile_assets(&self) -> &std::path::Path;
}
```


### `pishoo/src/chat.rs`

```rust
pub(crate) struct Chat {
    owner: Owner,
    store: ChatStore,
    access: Arc<AccessService>,
    outbound: RwLock<Option<Arc<dyn outbound::OutboundTransport>>>,
    worker_notify: Arc<Notify>,
    worker_shutdown: CancellationToken,
    worker_handle: StdMutex<Option<JoinHandle<()>>>,
}

impl Chat {
    pub(crate) fn new(
        owner_name: String,
        owner_subject_id: SubjectId,
        store: ChatStore,
        access: Arc<AccessService>,
    ) -> Self;
    pub(crate) fn start_worker(self: &Arc<Self>);
    pub(crate) fn wake_worker(&self);
    pub(crate) async fn shutdown(&self);
    pub(crate) async fn remote_chat_grants(
        &self,
    ) -> Result<Vec<(String, Vec<u8>, bool)>, sea_orm::DbErr>;
    pub(crate) async fn configure_outbound(&self, outbound: Arc<dyn outbound::OutboundTransport>);
    pub(crate) async fn update_remote_chat_grant(
        &self,
        contact_name: &str,
        subject_id: &SubjectId,
        granted_access: &GrantedAccess,
    ) -> Result<(), sea_orm::DbErr>;
}

impl Drop for Chat {
    fn drop(&mut self);
}

struct ChatContext {
    owner_name: String,
    capability: &'static str,
    endpoints: &'static [capabilities::CapabilityEndpoint],
}

pub(crate) fn router(state: Arc<Chat>) -> Router;
```


### `pishoo/src/chat/actor.rs`

```rust
pub struct Owner {
    name: String,
    subject_id: SubjectId,
}

impl Owner {
    pub fn new(name: String, subject_id: SubjectId) -> Self;
    pub fn ensure(&self, visitor: Option<&Visitor>) -> Result<(), StatusCode>;
    pub fn name(&self) -> &str;
}
```


### `pishoo/src/chat/bridge.rs`

```rust
pub(crate) struct MessageQuery {
    after: Option<String>,
    limit: Option<u16>,
}

pub(crate) struct SendMessage {
    text: String,
}

pub(crate) async fn get_messages(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<access_control::Visitor>>,
    Path(name): Path<String>,
    Query(query): Query<MessageQuery>,
) -> Result<Json<ConversationPage>, ServiceError>;

pub(crate) async fn post_message(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<access_control::Visitor>>,
    Path(name): Path<String>,
    body: Result<Json<SendMessage>, JsonRejection>,
) -> Result<(StatusCode, Json<service::LocalMessage>), ServiceError>;

pub(crate) async fn requeue_message(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<access_control::Visitor>>,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<service::LocalMessage>, ServiceError>;

pub(crate) async fn get_capability(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<access_control::Visitor>>,
    Path(name): Path<String>,
) -> Result<Json<service::CapabilityState>, ServiceError>;
```


### `pishoo/src/chat/capabilities.rs`

```rust
pub(crate) struct CapabilityEndpoint {
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
}

pub(crate) struct ChatCapability;

impl ChatCapability {
    pub(crate) const fn id(self) -> &'static str;
    pub(crate) const fn version(self) -> &'static str;
    pub(crate) const fn endpoints(self) -> &'static [CapabilityEndpoint];
    pub(crate) fn requested_access(self) -> RequestedAccess;
    pub(crate) fn offers(self) -> GrantedAccess;
    pub(crate) fn fixed_rules(self) -> [(AccessMethod, &'static str, Effect); 1];
    pub(crate) fn matches(self, method: &Method, path: &str) -> bool;
}
```


### `pishoo/src/chat/message.rs`

```rust
pub(crate) struct MessageSubmission {
    pub(crate) client_message_id: String,
    pub(crate) text: String,
}

pub(crate) struct MessageEnvelope {
    pub(crate) id: String,
    pub(crate) client_message_id: String,
    pub(crate) sender: String,
    pub(crate) recipient: String,
    pub(crate) text: String,
    pub(crate) created_at: i64,
}

pub(crate) struct MessageQuery {
    pub(crate) after: Option<String>,
    pub(crate) limit: u16,
}

pub(crate) enum MessageValidationError {
    EmptyClientMessageId,
    InvalidClientMessageId,
    EmptyText,
    TextTooLong,
    TextControlCharacter,
    InvalidCursor,
    CursorTooLong,
    InvalidLimit,
}

impl fmt::Display for MessageValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result;
}

impl MessageSubmission {
    pub(crate) fn validate(&self) -> Result<(), MessageValidationError>;
}

impl MessageQuery {
    pub(crate) fn new(
        after: Option<String>,
        limit: Option<u16>,
    ) -> Result<Self, MessageValidationError>;
}
```


### `pishoo/src/chat/messages.rs`

```rust
pub(crate) async fn post(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<Visitor>>,
    body: Result<Json<MessageSubmission>, JsonRejection>,
) -> Result<(StatusCode, Json<MessageEnvelope>), ApiError>;
```


### `pishoo/src/chat/outbound.rs`

```rust
pub(crate) struct RemoteResponse {
    pub(crate) status: StatusCode,
    pub(crate) body: Bytes,
}

pub(crate) trait OutboundTransport: Send + Sync {
    fn request<'a>(
        &'a self,
        target: &'a str,
        expected_subject_id: &'a SubjectId,
        method: Method,
        path: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<RemoteResponse, String>>;
}
```


### `pishoo/src/chat/service.rs`

```rust
static NEXT_CLIENT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct LocalMessage {
    pub(crate) id: String,
    pub(crate) client_message_id: String,
    pub(crate) remote_message_id: Option<String>,
    pub(crate) direction: String,
    pub(crate) state: String,
    pub(crate) sender: String,
    pub(crate) recipient: String,
    pub(crate) text: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) error_message: Option<String>,
}

pub(crate) struct ConversationPage {
    pub(crate) items: Vec<LocalMessage>,
    pub(crate) next_cursor: Option<String>,
}

pub(crate) struct CapabilityState {
    pub(crate) capability: &'static str,
    pub(crate) status: &'static str,
    pub(crate) contact_status: ContactStatus,
    /// Whether the local profile may enqueue messages for this contact.
    pub(crate) can_send: bool,
    pub(crate) can_receive: bool,
    /// Most recently observed permission on the remote profile. `None` means
    /// that this profile has not observed a `/std/contact/self` response yet.
    pub(crate) remote_grant: Option<bool>,
    pub(crate) endpoints: &'static [super::capabilities::CapabilityEndpoint],
}

pub(crate) fn require_owner(state: &Chat, visitor: Option<&Visitor>) -> Result<(), ServiceError>;

pub(crate) async fn resolve_target(state: &Chat, raw_name: &str) -> Result<String, ServiceError>;

pub(crate) async fn capability_state(
    state: &Chat,
    raw_name: &str,
) -> Result<CapabilityState, ServiceError>;

pub(crate) async fn list_messages(
    state: &Chat,
    target: &str,
    after: Option<String>,
    limit: Option<u16>,
) -> Result<ConversationPage, ServiceError>;

pub(crate) async fn send_message(
    state: &Chat,
    target: &str,
    text: String,
) -> Result<LocalMessage, ServiceError>;

pub(crate) async fn requeue_message(
    state: &Chat,
    target: &str,
    raw_id: &str,
) -> Result<LocalMessage, ServiceError>;

pub(crate) fn storage_error(error: DbErr) -> ServiceError;

pub(crate) fn now() -> Result<i64, ServiceError>;
```


### `pishoo/src/chat/store.rs`

```rust
pub enum StoreError {
    #[snafu(display("failed to create Chat database directory `{}`", path.display()))]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to open Chat database `{}`", path.display()))]
    Connect {
        path: PathBuf,
        source: sea_orm::DbErr,
    },
    #[snafu(display("failed to migrate Chat database `{}`", path.display()))]
    Migration {
        path: PathBuf,
        source: sea_orm::DbErr,
    },
    #[snafu(display("unsupported Chat database version {version} in `{}`", path.display()))]
    UnsupportedVersion { path: PathBuf, version: i64 },
}

pub struct ChatStore {
    db: DatabaseConnection,
}

impl ChatStore {
    pub async fn open(profile: &IdentityProfile) -> Result<Self, StoreError>;
    pub(crate) fn db(&self) -> &DatabaseConnection;
}
```


### `pishoo/src/chat/worker.rs`

```rust
static NEXT_LEASE: AtomicU64 = AtomicU64::new(1);

struct Job {
    id: i64,
    contact_name: String,
    message_id: Option<i64>,
    attempt_count: i64,
    lease_token: String,
}

pub(crate) fn spawn(
    state: Weak<Chat>,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
) -> JoinHandle<()>;
```

# Pishoo 接口约定

日期：2026-09-22。状态：**设计约定，尚未实现。** 上游以 [dhttp 顶层接口稿](../../dhttp/docs/api/top-level-review.md) 为准；本轮只对齐文档，不核对 dhttp 代码。

## 1. 主流程

Pishoo 按身份目录装配应用：发现 Server → 读取各自 config.db → 初始化共享 Network → 为各身份装配范围准入、Endpoint 和 Router。

```text
servers/<name>/config.db → 当前身份的 settings + proxy_locations
servers/<name>/apps/     → 当前身份的 WASM/OpenAPI
servers/<name>/public/   → 当前身份的静态内容
                          ↓
                   当前身份的 Router / Endpoint
                          ↓
                     共享 Network
```

实例根目录不设 pishoo.db。各 Server 的 config.db 保存监听范围与代理规则；daccess 权限仍放在该身份 db/access.db。代理表为空不影响静态和 WASM 服务。直接复用 dhttp Endpoint、Network 和标准 Router，不建立第二套运行时框架。

## 2. Pishoo 只需要的装配函数

以下是模块间函数签名，不是已实现代码；`Result` 使用项目统一错误处理。

```rust
// 直接对应数据库的一行；实现时可复用 ORM 实体，不再复制一套模型。
pub(crate) struct ProxyLocation {
    pub location: String,
    pub proxy_pass: String,
}

// daemon 主流程。
pub(crate) async fn run(state_dir: &Path) -> Result<()>;

// 一个 config.db 的一致配置快照。
pub(crate) struct ServerConfig {
    pub listen: u8, // 数据库 INTEGER：0=off, 1=internal, 2=external, 3=both
    pub proxy_locations: Vec<ProxyLocation>,
}

pub(crate) async fn load_server_config(
    server_dir: &Path,
) -> Result<ServerConfig>;

pub(crate) async fn discover_servers(servers_dir: &Path)
    -> Result<Vec<String>>;

// 汇总已校验身份的监听范围，只用于共享 Network 的底层资源准备。
pub(crate) fn network_config(configs: &[ServerConfig]) -> Result<dhttp::NetworkConfig>;

// 返回已经装配完整 middleware 和各服务分支的 Axum Router。
pub(crate) async fn build_router(
    server_dir: &Path,
    endpoint: &dhttp::Endpoint,
    locations: &[ProxyLocation],
) -> Result<axum::Router>;
```

`load_server_config` 打开该 Server 的 config.db，在一致读事务中读取 settings 和 proxy_locations；settings 必须恰好一行，listen 为 0..=3 的整数，显式映射到 dhttp Scopes。`discover_servers` 返回规范化且无冲突的目录名称。`build_router` 只接收当前身份的代理规则；扫描 public 和 apps 的部署文件并验证快照，同身份各 App 独享 data。首次构建时，单个 App 校验或加载失败只报告并跳过该 App。

启动流程伪代码（范围准入接缝尚待 dhttp 对齐）：

```text
names = discover_servers(state_dir/servers)
configs = 逐身份读取 config.db，校验失败的身份报告并跳过
network = DhttpNetwork::init(network_config(configs))
for each 有效身份及其配置:
    endpoint = Endpoint::load(name)
    router = build_router(server_dir, endpoint, config.proxy_locations)
    若允许入站:
        配置目标 Endpoint 的范围准入和发布约束  // 上游待补
        并发启动 endpoint.listen(router)
```

共享 Network 使用各身份范围并集不能替代逐身份限制；上游接缝完成前不支持混合范围上线。监听必须并发启动，daemon 保留任务并回收；单身份坏库只阻止该身份启动，单个坏组件只跳过对应 App。全局初始化失败需回收已启动资源。

## 3. Router 的内容

底层标准 HTTP/WASI 转换及内部执行结构见 [WASM HTTP 适配层](wasm-http-adapter.md)。

`build_router` 完成这些装配，不增加一套 handler trait：

- 代理行使用精简 Nginx location：先 `= /path` 精确匹配，再最长字符串前缀匹配；query 不参与匹配。
- 静态文件挂到公开文件路径，目录只尝试 index.html。
- 每个 `.wasm` 经 `dhttp::WasmApp` 加载；Pishoo 校验配套 OpenAPI，将其绑定到同一个身份沙盒，按声明的 API 逐条挂入 Router，并在调用 guest 前执行该 API 的访问策略。
- 所有 WASM API 挂到 `/api/<AppId>`，包括 `/api/index`；OpenAPI 保留应用内路径。`/api` 命名空间拒绝静态/代理占用，未声明入口返回 404；保留管理路径优先匹配。
- 最外层统一放 Server 身份检查、总资源限制、日志和取消；WASM 分支共享身份资源总账户、分别取得当前 App 的目录能力，按宿主策略配置读写和出站能力。

Axum 是 Pishoo 的应用依赖；dhttp 只要求 Tower Service。服务边界使用 `http::Request<dhttp::Body>` 和标准 HTTP response body，dhttp 客户端的 `Response<R>` 不混入路由接口。

身份直接读取 dhttp 提供的 `HandshakeSummary` / `ArcConnection` extensions。本端名称必须与该 Router 绑定的 Endpoint 一致，authority 不匹配返回 421；缺少可信握手信息按接入错误处理，不能当作匿名。WASM 分支先匹配声明的 API，再调用每身份 daccess 授权。普通 OpenAPI 不需要 Pishoo 扩展；可选默认权限建议只在受信任部署中初始化规则，缺失时采用 daccess 基础策略。静态、代理仍只允许自身访问，管理路径保留专门授权。guest 的 `pishoo-*` header 由 Pishoo 清洗后注入，其中包含 server name、app id 和可选 client identity；这些字段不能用于宿主鉴权。

首版权限管理由前端以真实用户身份直接调用 daccess 完成；WASM 不获得 ACL 写入、审批决定或间接授权的联系人管理能力，也不能通过 outgoing/自调用绕过限制。业务结果驱动的自动授权留作后续按需扩展，见 [权限管理边界](wasm-sandbox-design.md#31-首版权限管理边界)。

App 使用 `apps/<AppId>/{app.wasm, openapi.json, data/}` 布局；只把 data 挂给 guest。监听忽略 data 内变化，更新保留该目录。

## 4. 更新和关闭

这些是 `run` 的内部行为，暂不为每个动作公开类型或方法：

| 事件 | 行为 |
| --- | --- |
| 代理配置变化 | 读取并校验该 Server config.db，重建该身份的 Router，复用身份沙盒及未改变的 WasmApp |
| public、App 目录或 app.wasm/openapi.json 变化 | 准备变化 App 的 OpenAPI 配对快照及 WasmApp；单个 App 失败时保留该 App 已发布版本，首次加载失败则跳过该 App；完整 Router 校验成功后替换，Router 级失败保留旧 Router |
| 删除单个 App 的 wasm/OpenAPI 配对 | 新 Router 移除其 API 并取消该 App 执行，其他 App 继续 |
| 新增身份目录 | 校验 config.db；若需扩展当前 Network 范围则等待重启，否则在逐身份准入接缝就绪后启动 |
| 监听设置变化 | 保持运行中范围，标记该身份待重启 |
| 删除身份目录 | 先关闭该身份的应用准入，再 stop_listening、取消该身份活动任务并 close |
| daemon 退出 | 停所有 Endpoint 的准入，使用同一 deadline 排空和 close，最后 shutdown 全局 Network |

需要热更新时，监听入口使用标准 `tower::service_fn` 读取当前 Router；Router 引用可用 `ArcSwap` 等已有工具保存。它只是内部装配方式，不定义 GatewayApp、EndpointApp、Publisher、Generation 或 ServerRegistry 公共模型。每请求固定一份 Router、请求能力快照及内容引用，旧响应保留所需资源直到完成。同身份所有 App 共享预算；每 App 独享持久 data，同一 App 旧新版本共用其目录，请求使用各自不可变能力快照；更新不能丢失在途请求或重置 Server 总额度。

原子性按单个身份保证，不承诺不同身份同一时刻切换。重建结果提交前确认目录仍存在、仍属于这次 Endpoint 生命周期且输入未过时；旧任务不能复活删除或同名重建后的身份。检查与替换放在同一个短提交步骤中，不阻塞删除与关闭。

Body/guest 的后续任务必须被跟踪，返回响应头不等于完成；具体 guard、每身份共享计数器和取消令牌留在实现内部。OpenAPI 配对检查、凭据路径、WASM 宿主设置及流式请求细节见 [dhttp 接入说明](dhttp-integration-contract.md#4-接入时补齐的细节)。

## 5. 配置写入

每 Server 的 config.db 包含 settings 和 proxy_locations(location, proxy_pass)。CLI 的 NAME 只用于定位对应身份库，代理 put 按规范化 location 表达式 upsert，例如 `/backend/` 和 `= /health`；remove 使用相同规范化规则。匹配和 proxy_pass 尾斜杠语义见 [location 约定](pishoo-wasm-db-redesign.md#32-精简-nginx-location)。代理规则提交后下一轮读取应用；监听范围修改重启生效。没有跨身份配置事务，也不再提供全局 proxy prune。

```text
pishoo [--state-dir PATH] init
pishoo [--state-dir PATH] run
pishoo [--state-dir PATH] server init NAME
pishoo [--state-dir PATH] server listen NAME internal|external|both|off
pishoo [--state-dir PATH] proxy list NAME
pishoo [--state-dir PATH] proxy put NAME LOCATION UPSTREAM
pishoo [--state-dir PATH] proxy remove NAME LOCATION
pishoo app validate apps/profile/app.wasm --openapi apps/profile/openapi.json
```

`app validate` 是拟定的离线检查入口，执行与运行时相同的 OpenAPI、摘要配对和 imports 静态校验，不读取 state-dir 或写数据库。现有框架和 OpenAPI 工具在构建时生成配套文档，无需 Pishoo SDK，不再设计嵌入私有 manifest 的 pack 命令。`init` 不覆盖已有数据；`run` 要求已初始化并持有实例锁。`server init` 仅在合法身份目录下显式创建 config.db，不覆盖已有库；`run` 不自动补库。CLI 参数错误返回 2，运行失败返回 1，成功返回 0。批量内容部署的磁盘事务协议另定，不把文件扫描当作多文件事务。

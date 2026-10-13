# Workspace/Chat 宿主能力

本轮完成宿主接缝；Workspace/Chat 业务尚未迁入 WASM，默认 Lib/App 安装和后台周期驱动尚未接入。现有原生 Workspace/Chat 继续运行。

身份使用 `pishoo:identity/request-identity@0.2.0`：

```wit
local: func() -> tuple<string, string>;
caller: func() -> option<tuple<string, string>>;
```

SDK 导出 `pishoo_sdk::identity`。tuple 为 `(name, owner_hash)`，hash 从可信 authority 的证书 SKI 提取，主人比较完整 tuple。匿名 HTTP 和标准 CLI 后台调用的 caller 都是 none；具名身份缺少合法 SKI 时 trap。旧 @0.1.0 接口兼容保留，signatures 不变。

标准 WASI HTTP outgoing 的默认网络实现关闭。普通加载的 Lib 使用 Outgoing::Denied；只有原生调用方持有当前 Endpoint、AccessService 与 API Router 时才能构造 Granted。清单、Lib 名字与 HTTP 头不能授予这些资源。本轮没有向普通组件开放网络，也没有增加特权配置开关。

Granted 只接受 HTTPS DHTTP authority；`~` 指本端，远端名称沿用 DHTTP 规范化和证书验证。本端在进程内交给调用方提供的原生 Router，HTTP 执行传递真实 HandshakeSummary，匿名仍是匿名。后台分发使用实际 LocalAuthority 让原生 daccess 派生 owner Visitor；没有伪造 caller。实际握手优先于 LocalAuthority，外带 Visitor 被移除。

远端使用当前 Endpoint 和联系人数据，经 DHTTP 发送。请求/响应使用标准流式 Body，上传和响应可同时进行；请求扩展和逐跳头清除，带请求 trailers 时拒绝。请求/响应正文各限 1 MiB，单次传输最多15秒，并遵循更短的 WASI 连接、首字节和字节间隔期限。业务路径、联系人申请回执、chat 准入和队列状态不在 hook 中处理。

后台入口使用同一 component 的标准 `wasi:cli/run` 导出。每次有独立 Store、私有 `/db`、现有 fuel/limiter 和 TaskTracker；15秒超时会取消并回收该次任务。业务重试和 lease 由未来 Lib 管理，周期调度待默认 bundle 接入。

原生 daccess API 变化：

| API | 变化 | 身份要求 |
| --- | --- | --- |
| GET /std/contacts | items 返回稳定 id 与 incoming；incoming 表示当前名称和 subject 曾有入站申请 | 现有 daccess 授权 |
| GET /std/contact/{name} | 返回 id；公共 ContactRecord/schema 不变 | 现有 daccess 授权 |
| GET /std/contact/self | 不带 application_id 的资源响应增加 id | 真实 visitor 与资源 subject 匹配 |
| PUT /std/contact/{name} | 创建/更新本机联系人资源 | 同名且同 subject 的本端 owner；仍经过外层 daccess |

PUT 输入示例：

```json
{
  "subject_id": "目标的 owner hash 文本",
  "class": "",
  "description": "Bob",
  "status": "active",
  "requested_access": {},
  "offers": {}
}
```

status 只允许 pending/active，默认 pending；expired_after 可选，默认新资源7天，重试保留已有期限。新建返回201，已有成功写入返回204；同一资源重试不刷新时间。变化的 pending、blocked、expired 资源返回409；状态/别名修改和删除仍走现有 PATCH/DELETE。id/incoming/granted_access/created_at/updated_at 不可写。资源创建和激活复用已有原生方法；激活失败时保留 pending，允许相同请求重试。PUT 不发网络、不轮询、不按 chat 能力授权；ACL 单独走原有 API。

已撤除 `/std/contact/{name}/application` 原生编排和 `/std/pishoo/identity` 原型。远端申请仍用原生 POST /std/contact、GET /std/contact/self，由 WASM 编排；没有新增 HTTP WIT 或身份请求头。

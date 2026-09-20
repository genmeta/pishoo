# pishoo Workspace 实施方案

更新时间：2026-09-20

## 1. 背景与纠偏

原始构想中的 `/workspace` 不是 daccess 的管理后台，而是围绕一个 identity profile
组织联系人、能力扩展、应用、审批和个人设置的统一工作台。daccess 只覆盖其中一部分：

- 联系人接收、联系人本地状态和精确权限；
- 访问规则；
- 命中 `review` 后的一次性审批。

此前 Workspace 只有“审批、联系人、访问规则”三页，并把整个产品信息架构等同于
daccess。后续必须改为“pishoo Workspace shell + 多个领域能力”，daccess 作为其中一个
后端服务继续独立演进。

本方案以原始资料 [source/_workspace 畅想.md](source/_workspace%20畅想.md) 为产品意图，
同时以当前代码为实现基线，不把构想中尚不存在的 Wasm、市场和消息模块描述成现成功能。

## 2. 本阶段目标

### 2.1 必须落地

1. 建立正确的 Workspace shell、侧边栏和稳定深链。
2. 完成联系人主流程：添加联系人、收到的申请、已发送申请、联系人列表和联系人详情。
3. 完成审批请求主流程，并支持“记住当前选择”。
4. 完成快捷设置：我的信息、权限集合模板和快捷导航。
5. 为 API 扩展和应用管理建立侧边栏入口与真实占位页面。

### 2.2 本阶段不实现

- Wasm 扩展运行时、扩展签名和沙箱权限；
- 模块市场、应用市场、下载、安装、升级和卸载；
- 消息、日历、日记、项目管理等具体模块；
- 联系人聊天界面；
- 对外公开个人资料的标准 API；
- `/admin` 兼容路由。

占位页面不得提供看似可用但实际无效的安装、启用或聊天按钮。

## 3. 当前能力基线

| 能力 | 当前状态 | 本阶段处理 |
| --- | --- | --- |
| 每 identity 独立 profile 目录、证书和 `server.conf` | 已有 | 复用 |
| profile-scoped `AccessService` 与 `db/access.db` | 已有 | 复用 |
| mTLS/DHTTP 身份解析并注入 `Visitor` | 已有 | 复用 |
| 联系人接收、列表、详情、批准、拉黑、恢复、删除 | daccess 已有 | 重组前端信息架构 |
| 主动向远端发送联系人申请 | 未实现 | pishoo 新增出站编排 |
| 一次性审批 allow/deny 与过期时间 | daccess 已有 | 复用并改善 UI |
| 审批“记住选择” | 未实现 | 扩展 daccess 原子操作 |
| 通用访问规则管理 | 已有 | 从一级导航移到上下文入口/高级设置 |
| 出站 QUIC connector，支持携带 profile identity | 控制面已有 | 联系人申请复用 |
| 静态文件、反向代理和配置 reload | 已有 | 后续扩展/应用生命周期可复用 |
| profile 昵称、头像、性别等资料模型 | 未实现 | Workspace 数据库新增 |
| 权限集合模板、快捷导航 | 未实现 | Workspace 数据库新增 |
| 扩展注册表、Wasm 运行时 | 未实现 | 本阶段仅占位 |
| 应用清单、应用 UI 挂载和市场 | 未实现 | 本阶段仅占位 |

关键代码基线：

- `pishoo/src/service/daccess.rs`：profile-scoped daccess 与认证 middleware；
- `pishoo/src/service/snapshot.rs`：Workspace、daccess API 与业务 Router 的组合；
- `gateway/src/control_plane.rs`：`ProvideConnector` 和带 identity 的出站 connector；
- `pishoo/src/hypervisor/in_process_plane.rs`、`pishoo/src/worker/remote_plane.rs`：
  root/worker 两种 connector 实现；
- `pishoo/src/service/runtime.rs`：profile 服务加载和 reload 基座；
- `pishoo/workspace/`：当前 Solid/Vite 前端。

## 4. 产品与领域边界

```text
pishoo Workspace
├── Workspace shell、导航、profile context
├── 联系人出站申请与已发送申请状态
├── 我的信息、权限模板、快捷导航
├── API 扩展/应用注册表（后续）
└── 聚合各领域页面
        │
        ├── daccess
        │   ├── 入站联系人及本地联系人状态
        │   ├── 访问规则
        │   └── 审批请求
        │
        └── pishoo control plane
            └── 带当前 profile identity 的出站 DHTTP connector
```

必须保持以下边界：

- daccess 不依赖 Workspace，也不负责出站网络连接；
- pishoo 不复制 daccess 的授权算法或直接把浏览器输入当作 `Visitor`；
- Workspace 静态页面不拥有安全状态，所有敏感写操作必须经过后端鉴权；
- API 扩展是对外提供 API 的能力模块，应用是只在 Workspace 内组合 API 的界面，两者
  不得混为同一种安装单元。

## 5. 信息架构与路由

### 5.1 一级侧边栏

按原始构想保持五个一级入口：

1. **联系人**：最上方，显示新申请数量徽标；
2. **API 扩展**：本阶段为可访问占位页；
3. **应用管理**：本阶段为可访问占位页；
4. **审批请求**：显示有效 pending 数量徽标；
5. **快捷设置**。

profile 头像、显示名称和 identity name 固定在侧边栏顶部；点击品牌/profile 区域进入
Workspace 首页。访问规则不再作为一级导航：按联系人维度的权限进入联系人详情，按
API 维度的权限未来进入扩展详情，完整规则表保留为快捷设置中的“高级访问控制”。

### 5.2 稳定路由

| 路径 | 页面 | 阶段 |
| --- | --- | --- |
| `/workspace/` | 首页：profile 摘要、待办摘要、快捷导航 | 本阶段 |
| `/workspace/contacts` | 联系人列表 | 本阶段 |
| `/workspace/contacts/new` | 添加联系人 | 本阶段 |
| `/workspace/contacts/requests` | 收到/发出的联系人申请 | 本阶段 |
| `/workspace/contacts/:name` | 联系人详情和联系人维度权限 | 本阶段 |
| `/workspace/extensions` | API 扩展占位 | 本阶段占位 |
| `/workspace/apps` | 应用管理占位 | 本阶段占位 |
| `/workspace/approvals` | 审批请求 | 本阶段 |
| `/workspace/settings/profile` | 我的信息 | 本阶段 |
| `/workspace/settings/permissions` | 权限集合模板 | 本阶段 |
| `/workspace/settings/shortcuts` | 快捷导航 | 本阶段 |
| `/workspace/settings/access` | 高级访问控制 | 迁移现有规则页 |

当前 `/workspace/access/reviews|contacts|policies` 是错误粒度下的临时路径；在尚未发布的
前提下直接替换，不增加兼容重定向。

### 5.3 Shell 交互原则

- 桌面使用稳定侧边栏，内容区根据功能决定单栏、主从双栏或表格布局；
- 联系人可以采用参考图中的“列表 + 详情”结构，但没有消息模块时只显示详情，不伪造聊天；
- 移动端五个一级入口可使用带文字标签的底部导航或抽屉，触控目标不小于 44px；
- 当前路由必须有明确激活状态，浏览器前进/后退和深链刷新必须保持可用；
- 徽标同时提供数字/文本语义，不只依赖颜色；
- 继续使用现有简洁、扁平、硬边框的视觉语言，但 Workspace 是操作型产品，不采用
  App Store 营销页结构或低信息密度的大卡片；
- 所有占位入口均可深链，并明确说明“尚未启用”和依赖能力。

## 6. 联系人设计

### 6.1 三类数据

联系人页面需要明确区分：

1. **收到的申请**：远端调用本 profile 的 `POST /contact` 后，由 daccess 保存；
2. **发出的申请**：本 profile 主动连接远端并调用其 `POST /contact`，由 pishoo 保存本地
   跟踪状态；
3. **联系人**：已经建立或正在维护的关系视图，可聚合 daccess 联系人和已完成的出站关系。

不能把 daccess 的入站 `contacts` 表直接解释为完整的双向联系人通讯录。

### 6.2 页面能力

#### 添加联系人

表单字段：

- 对方 DHTTP 名称；
- 简介；
- 申请有效期，默认 3 天且不超过协议建议的 7 天；
- 请求的权限，可从权限集合模板选择后微调；
- 向对方声明的 `offers`，可为空。

提交期间按钮不可重复触发；名称、过期时间和权限错误必须显示在对应字段附近。

#### 联系人申请

- “收到的”与“已发送的”分栏；
- 收到的申请支持查看详情、配置联系人维度权限后批准、删除；
- 已发送申请显示 pending/active/expired/error、最后检查时间和手动刷新；
- 第一阶段不建立无限后台重试，状态更新由页面刷新或显式“检查状态”触发；
- 过期申请保留审计信息，不伪装成有效联系人。

#### 联系人列表与详情

- 展示名称、别名、类型、状态、添加/更新时间和有效期；
- 类型沿用并规范化为 Human、Agent、Robot、Service、Admin；未知值按原文展示；
- 没有头像时使用稳定的名称首字母占位，不远程抓取不可信图片；
- 详情展示 requested access、当前 granted access 和 offers；
- 联系人维度的访问规则在详情中编辑，底层仍写入 daccess `access_rules`；
- 消息模块未安装时不进入空白聊天页，显示“消息功能未安装”或“无消息权限”；安装后
  再由应用能力把联系人动作路由到消息应用。

### 6.3 出站联系人请求链路

```text
Workspace POST /workspace-api/contact-requests
    -> 校验当前 actor 是 profile owner
    -> 载入当前 profile Identity
    -> ProvideConnector(identity = current profile)
    -> 对目标名称发送已认证 POST /contact
    -> 成功后写入 workspace.db/outbound_contact_requests
    -> 返回本地 request id 与状态

Workspace 手动刷新
    -> 使用同一 profile Identity 调用远端 GET /contact/self
    -> 更新 active/pending/expired/error 与 last_checked_at
```

需要为 root in-process 和 worker IPC 两种运行模式提供同一 connector factory 抽象；不得
让浏览器上传证书、私钥或伪造 identity header。

### 6.4 Workspace API 草案

```text
POST   /workspace-api/contact-requests
GET    /workspace-api/contact-requests
GET    /workspace-api/contact-requests/{id}
POST   /workspace-api/contact-requests/{id}/refresh
DELETE /workspace-api/contact-requests/{id}
```

现有 daccess `/contacts`、`/contact/{name}` 和 `/contact/self` 协议保持不变。

## 7. 审批请求设计

### 7.1 一次性决定

沿用当前语义：管理员选择 allow/deny 和决定有效期，请求方查询状态并重试原请求，决定
只消费一次。

### 7.2 “记住当前选择”

Checkbox 对应的是明确的决定模式，而不是无限延长一次性审批：

- 未勾选：只更新当前 review；
- 勾选：同时为“该 visitor + 当前 method + 当前规范化 API”写入持久 allow/deny 规则，
  并正常处理当前 review。

UI 必须在提交前展示将被记住的准确范围，例如：

```text
以后对 alice.example 的 GET /calendar/events 请求始终允许
```

daccess 需要提供单个原子服务操作，在一个数据库事务中完成：

1. 验证 review 仍为有效 pending；
2. 必要时执行最后一位管理员保护；
3. upsert 精确访问规则；
4. 将当前 review 更新为 allow/deny 及决定有效期；
5. 提交后同步内存 Policies。

两个管理员并发处理时仍只能有一个成功。`reason = subject_id changed` 的审批在主体转让
协议完成前不得显示“记住选择”，因为普通 allow 规则不能安全替代 SubjectId 确认。

API 建议把决定范围设计为枚举，避免以后继续增加布尔字段：

```json
{
  "id": 12,
  "action": "allow",
  "expired_after": "2026-09-20T12:00:00Z",
  "mode": "once"
}
```

`mode` 第一阶段支持 `once` 和 `remember_exact`。

## 8. 快捷设置设计

快捷设置是 pishoo Workspace 的 profile-local 数据，不进入 daccess 数据库。

### 8.1 我的信息

- identity name：来自证书/profile，只读；
- 显示名称、头像、性别或自定义称谓：Workspace 本地资料；
- 头像限制 MIME、尺寸和文件大小，使用随机/固定内部文件名原子替换；
- 第一阶段这些资料只用于本地 Workspace 展示，不自动公开给联系人；
- 对外公开资料以后由标准 profile API 扩展显式提供，避免“修改本地头像”等同于公开数据。

### 8.2 自定义权限集合

权限集合是添加联系人时使用的模板，不是已经生效的访问规则。一个模板包含：

- 名称和说明；
- `requested_access`：API + methods；
- `offers`：API + effect + methods；
- 创建和更新时间。

应用模板后允许在提交联系人申请前微调，修改后的内容不反向覆盖模板。

### 8.3 快捷导航

- 保存入口类型、目标 id、显示名称、顺序和启用状态；
- 第一阶段只允许选择真实存在的 Workspace 内部页面；
- API 扩展/应用注册表落地后，再允许选择已启用的应用；
- 首页在宽屏右侧或下方展示快捷入口，移动端按内容优先级折叠；
- 删除目标功能时必须清理或标记失效快捷项，不能留下死链接。

### 8.4 Workspace API 草案

```text
GET/PATCH /workspace-api/settings/profile
GET/POST  /workspace-api/settings/permission-sets
PATCH/DELETE /workspace-api/settings/permission-sets/{id}
GET/PUT   /workspace-api/settings/shortcuts
```

这些 API 默认仅允许当前 profile owner；未来如需委托管理员，必须引入单独能力而不是
复用普通联系人访问规则。

## 9. Workspace 数据库

建议新增 `<profile>/db/workspace.db`，避免把 UI 偏好和出站编排状态写入
`db/access.db`。初始 schema：

```text
module_versions
profile_preferences
permission_sets
permission_set_requests
permission_set_offers
workspace_shortcuts
outbound_contact_requests
```

关键约束：

- 每个 profile 单独连接，禁止跨 profile 共享；
- 时间统一为 UTC Unix 秒；
- 权限 API 路径和 method 使用 daccess 相同的规范化与校验规则；
- `outbound_contact_requests` 对 target name + 当前未终结申请建立约束，避免重复发送；
- 敏感写入使用事务和原子文件替换；
- 数据库迁移由 pishoo Workspace store 自己管理，不复用 daccess schema version。

## 10. API 扩展与应用管理占位

### 10.1 API 扩展占位页

页面说明未来模型：一个扩展拥有一组 API、一个隔离数据库、一个版本化 manifest 和可选
Wasm 组件；展示“当前尚未启用扩展运行时”，不显示虚假的安装/卸载按钮。

稳定路由和导航本阶段先落地，后续必须另行完成 RFC，至少定义：

- manifest 与 API namespace；
- Wasm component ABI、资源配额和 host capability；
- 每扩展数据库生命周期与 migration；
- 签名、来源、安装、升级、回滚和卸载；
- 扩展 API 如何进入 daccess 规则与审计。

### 10.2 应用管理占位页

页面说明应用是 Workspace 内部 UI，对外不暴露应用自己的界面路由；应用可以组合一个或
多个 API 扩展。后续 RFC 至少定义：

- 应用 manifest、静态资源和前端隔离方式；
- 依赖的 API 扩展及版本约束；
- Workspace 路由、导航注册和快捷入口；
- 安装来源、签名、CSP、升级、禁用和卸载；
- 应用只拥有用户已授予的本地 API capability。

## 11. 后端结构建议

```text
pishoo/src/workspace/
  mod.rs              # WorkspaceState 与 router
  actor.rs            # owner/admin 身份边界
  context.rs          # profile context、功能状态和徽标
  contacts.rs         # 出站申请编排
  settings.rs         # profile、权限模板、快捷导航 API
  store.rs            # workspace.db 与 migration
  outbound.rs         # profile-authenticated DHTTP client abstraction

pishoo/src/service/
  daccess.rs           # daccess 组合，逐步移除 Workspace-specific context
  workspace.rs         # 静态资源 service
```

`WorkspaceState` 至少持有 profile、owner identity、workspace store、`AccessService` 和
出站 connector factory。静态资源 service 不持有这些状态。

## 12. 前端结构建议

```text
pishoo/workspace/src/
  app/
    WorkspaceShell.tsx
    routes.ts
    navigation.ts
  features/
    home/
    contacts/
    approvals/
    settings/
    extensions/       # 占位
    apps/              # 占位
    access/            # 高级规则管理
  shared/
    api/
    components/
    i18n/
    styles/
```

当前按页面平铺的 `src/pages` 可以分阶段迁移，避免一次性重写。浏览器请求继续集中在 API
client；daccess 与 Workspace API 类型分模块维护，不在组件中直接散落 `fetch`。

## 13. 实施阶段

### M0：Shell 与路由纠偏

- [ ] 建立五个一级侧边栏入口、profile header、徽标位置和移动端导航；
- [ ] 新增 Workspace 首页；
- [ ] 将联系人、审批、规则页面迁到新路由；
- [ ] 新增 API 扩展和应用管理占位页；
- [ ] 保证深链、前进/后退、键盘导航和 375/768/1440px 布局。

### M1：Workspace store 与 owner API

- [ ] 新增 profile-scoped `workspace.db` 和 migration；
- [ ] 建立严格 owner actor；
- [ ] 实现 profile preferences、permission sets 和 shortcuts CRUD；
- [ ] 扩展 context/bootstrap 响应，提供 profile 信息、功能状态和徽标计数。

### M2：联系人已有能力重组

- [ ] 将 daccess 联系人列表拆为联系人/收到申请视图；
- [ ] 完成联系人详情与联系人维度权限入口；
- [ ] 增加类型、头像 fallback、时间和状态展示；
- [ ] 保留批准、拉黑、恢复、删除和并发错误反馈。

### M3：主动添加联系人

- [ ] 为 root/worker 模式提供统一 profile connector factory；
- [ ] 实现出站申请表和 `/workspace-api/contact-requests`；
- [ ] 实现模板选择、发送、已发送列表和手动状态刷新；
- [ ] 增加远端失败、超时、重复提交和过期测试；
- [ ] 使用真实 DHTTP/mTLS listener 做双 profile 端到端测试。

### M4：审批“记住选择”

- [ ] 在 daccess 增加 `once|remember_exact` 决定模式；
- [ ] 原子处理 review、规则、管理员存续和内存 Policies；
- [ ] SubjectId 变化审批禁用 remember；
- [ ] Workspace 增加 Checkbox、准确范围说明和错误恢复；
- [ ] 增加并发、过期、一次性消费和后续规则命中测试。

### M5：快捷设置与首页

- [ ] 完成我的信息、头像上传及本地展示；
- [ ] 完成权限集合模板编辑与添加联系人联动；
- [ ] 完成快捷导航排序、启停和首页呈现；
- [ ] 保留高级访问控制入口；
- [ ] 完成中英文、无障碍和响应式验收。

### M6：占位边界验收

- [ ] API 扩展和应用管理入口、路由、标题、说明完整；
- [ ] 占位页不出现不可用的操作；
- [ ] 建立后续两个独立 RFC 的链接位置；
- [ ] 确认本阶段未引入 Wasm/runtime/marketplace 的隐式承诺。

## 14. 安全与一致性要求

- 所有身份来自 pishoo 已验证的 DHTTP/mTLS connection，不接受浏览器声明的身份；
- 添加联系人使用当前 profile 私钥只发生在 pishoo 控制面，不把密钥交给 Workspace；
- profile settings、扩展/应用生命周期默认为 owner-only；
- 联系人和审批管理继续遵守 daccess ACL、默认 deny 和最后一位管理员保护；
- remember 规则的范围必须在 UI 与数据库中完全一致，禁止隐藏扩大到通配 API；
- 头像和未来应用静态资源必须防止路径穿越、MIME 欺骗和无限大小上传；
- root/worker 两种运行模式必须保持相同语义，不允许只在 in-process 模式可用；
- 所有列表分页，异步请求可取消，旧请求不得覆盖新状态；
- 删除、卸载和永久拒绝等破坏性操作必须确认并提供明确影响说明。

## 15. 验证矩阵

| 范围 | 最低验证 |
| --- | --- |
| Workspace shell | 深链、back/forward、active state、键盘、375/768/1440px |
| 联系人接收 | daccess 定向测试 + Workspace E2E |
| 联系人发送 | 两个隔离 profile 的真实 DHTTP/mTLS E2E |
| 审批一次性决定 | pending/status/retry/consume 定向测试 |
| 审批 remember | 原子规则写入、并发、过期、SubjectId change 测试 |
| 快捷设置 | migration、owner 隔离、CRUD、头像校验测试 |
| root/worker | connector 与 Workspace API 行为一致性测试 |
| 占位页 | 无虚假 CTA、路由可访问、文案和无障碍检查 |

未经明确授权，不用全 workspace 编译代替定向验证。

## 16. 待确认但不阻塞 M0 的问题

1. “我的信息”中的性别字段是否采用自由文本、枚举还是完全可选的自定义称谓？
2. 本地个人资料未来由哪个标准 API 扩展对外公开，公开字段是否逐项授权？
3. 联系人关系是否要求双方都 active 才进入统一联系人列表，还是单向批准即可？
4. 发出申请成功后是否需要后台低频轮询，还是长期保持手动刷新？
5. API 扩展的 Wasm 目标采用 WASI Preview 2/Component Model 还是其他 ABI？
6. 应用 UI 采用同 bundle 编译、动态模块还是 iframe/独立 origin 隔离？

M0 到 M2 可以在这些问题未定时推进；M3 需要确认联系人关系语义，扩展和应用的正式
施工必须先完成各自 RFC。

## 17. 本次文档完成标准

- 原始文档和两张图片在本目录完整归档；
- Workspace 与 daccess 的边界得到纠正；
- 当前 pishoo 能力和缺口有明确矩阵；
- 联系人、审批和快捷设置具备可实施的数据、API、页面与阶段设计；
- API 扩展和应用管理只承诺侧边栏与占位路由；
- 后续开发可以按 M0→M6 分阶段提交和验收。

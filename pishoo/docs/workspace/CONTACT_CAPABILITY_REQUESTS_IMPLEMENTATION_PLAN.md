# 联系人与能力请求模型实施计划

更新时间：2026-09-24  
状态：M0/M1/M2/M7 已落地，M3/M4 进行中

本轮修订：Chat 采用单一“消息投递”能力。远端只开放 `POST /std/message`；消息历史由接收方本地 `chat.db` 保存并读取，不引入 `chat.read` 能力。

## 1. 决策摘要

本计划调整联系人、能力和访问审批之间的关系：

1. 联系人是 identity 与能力授权状态的聚合视图，不再要求用户单独批准一个没有权限含义的“联系人关系”。
2. 联系人申请是能力请求的承载形式。首次申请可以同时请求建立联系人记录、请求对方能力和向对方开放本地能力。
3. 能力授权按能力 ID 和版本作出一次决定。能力包含的多个 API 由服务端根据 descriptor 展开为底层访问规则，用户不逐个审批 API。
4. 联系人能力具有方向性：`我允许对方投递消息` 与 `我请求对方提供能力` 是两个独立状态。Chat 的远端能力只代表消息投递，不代表读取对方消息。
5. 具体 method + API 的一次性访问审批继续保留，但它属于“访问审批”，不属于能力申请。
6. 后端仍保留联系人实体，用于绑定 identity、SubjectId、阻止、过期、撤销和审计；这个实体不产生额外的用户审批步骤。

推荐的信息架构：

```text
请求中心
├── 能力请求
│   ├── 首次联系人能力申请
│   └── 已建立联系人新增能力申请
└── 访问审批
    ├── GET /files
    └── POST /calendar/events

联系人
└── 展示 identity、联系人信息和双方能力状态
```

## 2. 当前实现的问题

当前实现已将联系人能力申请与底层访问规则分开：

- Workspace API 和表单保存 `requested_capabilities` 与 `offered_capabilities` 两个方向；
- 服务端按 descriptor 生成 daccess `/contact` 所需的低层规则，低层字段不作为 Workspace 请求模型；
- 联系人详情使用按能力授予入口，`/acl/reviews` 仍只处理具体 method + API 的访问审批；
- Chat 已收敛为远端 POST 投递，消息记录只从本地 Chat 数据库读取。

底层把 capability 展开为 API 规则是必要的实现步骤，但这个展开不应成为用户界面的审批单位。

## 3. 术语和边界

### 3.1 Identity

经过 mTLS 验证的远端主体。identity name 与 SubjectId 共同绑定一次授权主体。

### 3.2 联系人记录

profile 本地保存的远端 identity 记录，包含名称、SubjectId、别名、资料和生命周期状态。
联系人记录本身不授予业务 API 访问权限。

### 3.3 能力

由模块声明的可理解授权单元，例如 `chat`、`calendar.read` 或 `files.read`。能力 descriptor 至少包含：

- `id`；
- `version`；
- 提供范围 `visibility`；
- 审批模式 `approval_mode`；
- 展示名称和用途；
- 所包含的固定 endpoint 集合。

endpoint 集合用于服务端校验、审计和生成规则，不作为用户逐项审批列表。

### 3.4 能力授权

某个 identity 在某个方向上是否获得某个 capability 的授权。授权至少绑定：

```text
contact identity
subject_id
direction
capability_id
capability_version
status
```

建议状态：

```text
requested → approved
requested → denied
approved  → revoked
requested → expired
```

### 3.5 访问审批

当已有访问策略将某个精确的 method + API 标记为 `review` 时，远端实际请求产生的一次性审批记录。
它可以决定“本次允许”“本次拒绝”或“记住这一条精确规则”，不代表能力申请。

## 4. 联系人生命周期

联系人记录继续保留内部状态，但不再要求用户单独批准关系：

| 内部状态 | 含义 | 用户界面表现 |
| --- | --- | --- |
| `pending` | 收到能力申请，尚无已生效的本地能力 | 请求中心中的待处理项 |
| `active` | 至少有一项能力已授权，或用户明确保存为本地联系人 | 联系人列表 |
| `blocked` | 本地阻止该 identity | 联系人列表中的阻止状态；所有本地能力撤销 |
| `expired` | 申请或联系人有效期结束 | 历史记录，不作为有效联系人 |

“已建立联系人”默认指拥有至少一项已生效能力的联系人。若产品需要保存没有任何能力的 identity，增加“仅保存身份”操作；该操作只创建本地通讯录记录，不改变远端访问策略。

拒绝所有能力时，保留申请审计记录并不创建 `active` 联系人。阻止 identity 是独立的安全操作，可以阻止后续能力申请。

## 5. 双向能力模型

每次联系人申请都要明确两个方向。

### 5.1 我希望对方提供给我

这是本地 identity 对远端能力的请求，例如：

```text
我希望 Alice 向我开放 Chat
```

该请求最终需要由 Alice 的 profile 决定。远端批准后，本地 worker 才可以向 Alice 的 Chat 投递消息。消息历史由本地 Chat 数据库提供给本地界面。

### 5.2 我愿意向对方开放

这是本地 profile 对远端 identity 的授权意向，例如：

```text
我愿意向 Alice 开放 Chat
```

本地批准后，daccess 为 Alice 写入 Chat 对应的固定访问规则。这个决定不会自动授予本地调用 Alice 的权限。

### 5.3 Chat 示例

Chat 在协议上仍然是方向性的：

```text
Alice → Bob：Alice 的 worker 是否可以向 Bob 的 `/std/message` 投递消息
Bob → Alice：Bob 的 worker 是否可以向 Alice 的 `/std/message` 投递消息
```

完整双向聊天需要双方分别授予 Chat 投递能力。前端可以提供“双方都申请 Chat”的快捷预设，但服务端仍保存两个独立方向的能力状态。

消息读取不走远端 Chat 能力：

```text
发送方浏览器
  → 发送方 chat.db
  → 发送方 Chat worker
  → 接收方 POST /std/message
  → 接收方 chat.db
  → 接收方浏览器读取本地消息
```

接收方不需要向发送方调用 `GET /std/message`。本地 `GET /chat-api/.../messages` 只查询当前 profile 的本地数据库。

## 6. 能力 descriptor 和规则展开

能力 descriptor 建议使用明确的审批模式：

```json
{
  "id": "chat",
  "version": "1",
  "visibility": "contact",
  "approval_mode": "capability",
  "endpoints": [
    { "method": "POST", "path": "/std/message" }
  ]
}
```

审批模式建议：

| 模式 | 含义 |
| --- | --- |
| `none` | 直接公开，不创建能力申请 |
| `capability` | 由一次能力决定生成整组固定规则 |
| `request` | 每次具体访问可能进入 method + API 访问审批 |

Chat 使用 `capability`。批准 Chat 时，服务端一次性校验 descriptor、SubjectId 和联系人状态，再只写入 `POST /std/message` 的精确投递规则。读取本地消息使用 owner-only 的 Chat bridge，不属于远端能力授权。

如果未来出现真正不同的业务权限，应按业务能力拆分；不要因为一个能力包含多个 HTTP method 就自动生成 `chat.read` 或 `chat.send`。Chat 当前只定义一个投递能力，endpoint 为远端 `POST /std/message`。不把底层 API 直接暴露为复选框，也不允许浏览器提交任意 API path、method 或 effect。

## 7. 请求中心设计

### 7.1 能力请求

首次联系人申请显示为一个能力请求卡片：

```text
Alice 申请建立能力联系

对方希望使用我的能力
  Chat                         [授予] [拒绝]

对方愿意向我开放的能力
  Chat                         [请求使用]

操作
  [授予选中能力] [拒绝选中能力] [阻止此 identity]
```

联系人关系不再单独显示“批准联系人”。若至少一项本地能力被授予，联系人自动进入联系人列表。

### 7.2 已有联系人新增能力

已有联系人申请新能力时，直接显示能力请求：

```text
Alice 请求新增能力：文件读取
申请理由：查看项目资料
[授予] [拒绝]
```

该流程不重新创建联系人关系，也不影响 Alice 已有的其他能力。

### 7.3 访问审批

访问审批与能力请求显示在同一审批列表中，以“类型”区分；两者仍使用各自的决定流程：

```text
Alice 请求 GET /files

[仅允许本次] [记住精确规则] [拒绝]
```

此处的“记住”只作用于当前 visitor + method + API + SubjectId 范围，不自动批准某个 capability。
审批中心按“待处理 / 已过期”切换；已过期页同时显示未获决定的过期能力请求和过期访问审批，只供查看，不提供授予或拒绝操作。已批准、已拒绝的记录不列入“已过期”。
联系人页面不再单列“收到的申请”：当前支持审批的入站 Chat 能力请求统一从审批中心处理，并可从申请方名称进入身份详情；“已发送申请”仍保留在联系人导航中。没有申请 Chat 的身份记录不构成 Chat 审批。

### 7.4 联系人详情

联系人详情展示能力矩阵，不重复显示请求中心的决定按钮：

| 能力 | 我向对方开放 | 对方向我开放 |
| --- | --- | --- |
| Chat | 已授权 | 待对方授权 |
| 文件读取 | 未授权 | 已授权 |

如果存在待处理申请，详情提供“在请求中心处理”的入口。

## 8. 添加联系人页面

页面分成两个能力方向：

```text
目标 identity
简介
申请有效期

我希望对方提供给我
  [ ] Chat
  [ ] 日历读取

我愿意向对方开放
  [ ] Chat
  [ ] 公开资料（通常不需要申请）
```

能力目录来自 descriptor。当前 profile 能提供的能力用于“我愿意向对方开放”；目标 profile 能提供的能力需要通过受控的远端 descriptor 发现，或者使用协议内置的标准能力目录。

远端能力目录尚未可用时，第一阶段可以先支持本地提供能力的选择，并将“请求对方能力”作为后续阶段；不能把本地能力自动复制成对方能力。

## 9. 后端边界

### 9.1 daccess

daccess 继续负责：

- identity 和 SubjectId 验证；
- 联系人记录、阻止和过期；
- 有效的精确 `access_rules`；
- method + API 访问审批；
- 最后一位管理员保护和策略缓存同步。

daccess 不负责解释 Chat、日历等业务能力名称。

### 9.2 Capability Registry / Provider

pishoo 控制面提供能力注册和授权编排：

- 注册 descriptor；
- 将能力 ID 解析为固定 endpoint 集合；
- 校验能力版本和方向；
- 调用能力所属模块执行 grant/revoke；
- 将能力授权转换为 daccess 固定规则；
- 保证 grant/revoke 操作幂等。

Chat、日历等模块拥有自己的能力状态和业务数据。Workspace 只展示 provider 返回的状态，不直接读模块数据库。

### 9.3 Workspace

Workspace 负责：

- 请求中心的聚合页面；
- 添加联系人和能力方向选择；
- 联系人详情能力矩阵；
- 请求中心徽标和状态反馈。

浏览器不提交任意路径、method、effect、SubjectId 或远端身份声明。

## 10. 数据和 API 计划

### 10.1 请求记录

请求记录只使用能力 ID，不保存浏览器提交的 API 规则：

```text
capability_requests
-------------------
id
contact_name
subject_id
direction                 -- inbound_grant / outbound_request
capability_id
capability_version
source_contact_request_id NULL
status                    -- pending / approved / denied / revoked / expired
reason
created_at
decided_at NULL
updated_at
```

有效能力授权由所属 provider 管理；请求记录用于请求中心、审计和幂等关联。daccess 的 `access_rules` 仍是实际访问授权的权威来源。

### 10.2 Workspace API 草案

```text
GET  /workspace-api/requests?kind=capability|access_review
GET  /workspace-api/requests/{id}
POST /workspace-api/capability-requests/{id}/decision
GET  /workspace-api/contacts/{name}/capabilities
POST /workspace-api/contacts/{name}/capabilities/{id}/grant
POST /workspace-api/contacts/{name}/capabilities/{id}/revoke
```

已实现的审批中心读取接口为 `GET /workspace-api/approvals?status=pending|expired&page=1&page_size=20`。服务端先聚合两类请求，再统一排序和分页；能力决定仍走 Chat capability 端点，访问决定仍走 `/acl/review`。

现有出站接口调整为表达两个方向：

```json
{
  "target_name": "alice.example",
  "description": "需要进行项目协作",
  "requested_capabilities": ["chat"],
  "offered_capabilities": ["chat"]
}
```

服务端根据 descriptor 生成 daccess `/contact` 协议的 `requested_access` 和 `offers`，并附上稳定的 `application_id` 交给后台 worker 投递。接收方以实际收到时间设定 7 天有效期；Workspace UI 不提供有效期字段。

### 10.3 访问审批 API

现有 `/acl/reviews` 和 `/acl/review` 继续服务于 method + API 访问审批。若未来需要在审计中显示能力来源，可在 review 记录增加可选的 `capability_id`，但这不改变访问审批的决定粒度。

## 11. 授权和撤销流程

### 11.1 授予能力

1. owner 验证当前 request 与联系人 SubjectId 一致；
2. provider 验证 capability ID、版本和方向；
3. provider 幂等写入能力授权状态；
4. daccess 原子更新该能力对应的精确规则；
5. 更新请求状态为 `approved`；
6. 联系人至少拥有一项本地有效能力时进入 `active` 视图。

### 11.2 拒绝能力

1. 请求状态变为 `denied`；
2. 不创建 allow 规则；
3. 不影响同一联系人已有的其他能力；
4. 可以允许之后重新提交新的能力申请。

### 11.3 撤销和阻止

- 撤销能力只删除该能力拥有的精确规则，不影响其他能力；
- 阻止联系人撤销该 identity 的所有本地能力，并拒绝后续联系人/能力申请；
- SubjectId 变化时，原能力授权全部进入待重新验证状态，不沿用旧主体的规则。

跨 Workspace、provider 和 daccess 数据库的操作必须使用可重试的幂等命令和补偿状态，不能依赖跨数据库事务。

## 12. 实施阶段

### M0：模型和协议固化

- [x] 确认联系人不再有独立用户审批；
- [x] 确认能力授权的两个方向；
- [x] 为 descriptor 增加 `approval_mode`、版本和展示元数据；
- [x] 明确 `requested_capabilities` 与 `offered_capabilities` 的协议语义；
- [x] 为 Chat 写出双向授权矩阵和状态样例。

### M1：能力方向拆分

- [x] 修改添加联系人 API 和表单，使请求能力与开放能力分开；
- [x] 停止 `selected_access()` 自动复制两个方向；
- [x] 服务端只允许 descriptor 中存在的 capability ID；
- [x] 通过 daccess `/contact` 完成联系人申请投递。

### M2：能力授权服务

- [x] 建立 Workspace capability registry 映射；当前注册 Chat，后续能力继续按 descriptor 扩展。
- [x] 将 Chat grant/revoke 从“批准联系人”流程中拆出；
- [x] Chat 能力批准只生成远端 `POST /std/message` 固定投递规则；
- [x] 增加 Chat 远端授权状态缓存与 SubjectId 校验；能力版本仍由 descriptor 固定为 v1。
- [x] grant/revoke 使用固定规则幂等更新；远端授权状态按刷新结果覆盖缓存。

### M3：请求中心

- [x] 请求中心聚合当前 Chat 联系人能力申请；新增能力继续沿用同一能力目录数据源；
- [x] 移除独立的“批准联系人”按钮；
- [x] Chat 能力请求支持授予和拒绝；拒绝按申请记录 ID 持久化决定并关闭当前仅含 Chat 的申请，之后重新申请可再次进入待处理列表；
- [x] 联系人详情展示双向能力矩阵；
- [x] 审批中心将能力请求与访问审批合并为一张列表，以“待处理 / 已过期”切换状态。
- [x] 移除联系人页重复的“收到的申请”列表，保留已发送申请和审批中心到身份详情的入口。

当前 Chat 拒绝会在 Workspace 数据库保存最新决定和追加式审计事件，再删除 daccess 中的待处理联系人记录。重新申请获得新的记录 ID；审批中心的授予和拒绝都校验当前记录 ID 与 descriptor 版本，旧页面的决定不会落到新申请上。此路径只处理仅申请 Chat 的记录；多能力申请需要后续改为逐能力保留联系人记录。

### M4：联系人视图重组

- [x] 联系人目录收敛为有效 Chat 能力、显式保存的身份及需要继续管理的阻止记录；无能力且未保存的 active 记录不再入列；
- [x] 当前仅含 Chat 的申请被拒绝时保留决定事件，且不显示为 active 联系人；
- [x] Chat 的阻止、撤销、SubjectId 变化和过期状态统一更新双向能力矩阵；其他能力随各自 provider 接入；
- [x] 调整导航徽标，使其统计能力请求和访问审批的待处理数。

Chat 远端授权缓存现在同时绑定联系人名与 SubjectId；旧缓存缺少 SubjectId 时视为未知。出站消息入队时记录收件人 SubjectId，worker 投递前核对本地联系人，出站连接还会核对 TLS 对端证书中的 owner hash；身份变化则阻止旧任务。旧消息不能对新身份手动重试，新的授权也不会重新排队旧身份的消息。`chat.db` 只接受当前队列 schema，版本不匹配时由部署流程重建 profile 数据库。

联系人目录由 Workspace 聚合当前有效的本地 Chat 规则、与 SubjectId 匹配的远端 Chat 授权缓存，以及用户显式保存的身份。本地 Chat 入列还要求 `approved` 决定匹配当前联系人记录 ID 和 SubjectId。保存身份不创建访问规则，保存标记也绑定记录 ID 与 SubjectId；删除后同名身份重新申请不会继承旧标记。被阻止的身份继续显示，以便恢复或删除。已保存的待处理申请也会保留在请求视图中，直到能力决定完成。

Workspace 联系人页统一使用一张列表，默认只显示状态为 active、拥有有效 Chat 能力、未标记本地保存且不属于需处理的正常联系人。前端提供“需处理 / 本地保存 / 已拉黑”分类筛选，不设置“全部”分类；初始无分类选中，点击分类只显示该分类，再次点击当前分类取消筛选并返回正常列表，切换或取消时重置分页和勾选。需处理包含未收录身份，以及申请过 Chat、但尚未确认对方授予我方发送权限的身份；本地保存依据显式保存标记；已拉黑依据联系人状态。同一身份可符合多个分类，但在当前视图中只出现一次，且属于任一分类的身份均不进入默认正常列表。未收录身份可在列表行内保存，状态列区分远端明确未授权与状态未知，不将未知解释为拒绝；需处理及本地保存身份不再同时显示绿色“正常”标签。
目录接口同时标记当前是否有有效 Chat 能力及与 SubjectId 绑定的远端 Chat 授权状态；联系人列表据此在详情入口旁提供聊天入口。更新时间和过期时间留在详情中，待处理申请列表仍显示时间以便判断时效。

`POST /std/message` 的接收入口在 daccess 放行后，还会核对精确 Chat 规则与绑定当前联系人记录 ID、SubjectId 的 `approved` 决定。同名身份更新 SubjectId 时，旧规则本身不会继续提供 Chat 接收权限。

远端能力以最近一次状态刷新或投递反馈为准；远端撤销授权后，本地需在下一次刷新或收到 403 时更新缓存，目录不会实时查询所有联系人。

### M5：远端能力发现和对称快捷项

- [ ] 定义受控的远端 capability descriptor 发现接口；
- [ ] 添加联系人页支持“我希望对方提供”的能力选择；
- [ ] 为 Chat 增加“双向 Chat”快捷预设；
- [ ] 远端能力不可发现时显示明确的降级状态，不复制本地能力目录。

### M6：访问审批边界和清理

- [x] 审批中心将 `/acl/reviews` 所代表的一次性访问审批标为“访问审批”类型，并与能力请求合并展示；
- [ ] 保留 `once` 和 `remember_exact`，禁止其隐式批准 capability；
- [ ] 清理联系人页、审批页中的重复决定入口；
- [ ] 更新中英文文案、帮助文本和深链。

### M7：Chat 投递模型收敛

- [x] 将远端 Chat descriptor 从 `GET/POST /std/message` 收敛为仅 `POST /std/message`；
- [x] 移除远端消息读取作为能力授权的概念；
- [x] Chat worker 发送时只调用接收方的 `POST /std/message`，权限由接收方授权层执行；
- [x] Chat bridge 的消息列表只查询本地 `chat.db`；
- [x] 清理远端 `GET /std/message`、remote cursor 和主动拉取同步任务；
- [x] 将接收消息定义为对方 worker 向本 profile 的 POST 投递，并保留本地幂等落库。

## 13. 验收标准

### 模型

- [x] 联系人关系没有独立的用户审批步骤；能力授予动作会在内部激活联系人以满足 daccess 生命周期。
- [x] 没有能力授权的 pending 申请不会产生业务 allow 规则；
- [x] 能力授权只影响该 capability 的固定 endpoint；
- [x] Chat 能力只影响远端 `POST /std/message`，不包含远端读取权限；
- [ ] 同一联系人可以分别拥有多个能力的 approved、denied、revoked 状态；当前仅实现 Chat grant/revoke。

### 方向

- [x] 授予 Alice 使用我的 Chat，不会自动授予我使用 Alice 的 Chat；
- [x] Chat bridge 暴露 `can_send` 与 `can_receive`，发送按钮按两个方向分别显示。
- [x] 发送消息先写入本地数据库，再由 worker 向对方 POST；
- [x] 接收消息由对方 POST 到本地后写入本地数据库；
- [x] 浏览器读取消息只访问本地 Chat bridge，不访问远端消息 GET；
- [x] requested/offered 能力在接口和界面中方向清楚。

### 访问审批

- [ ] method + API review 不会自动变成 capability grant；
- [ ] `remember_exact` 只生成精确规则；
- [x] 访问审批和能力请求在审批中心共用列表，以类型区分并保留各自的操作。

### 安全

- [ ] 浏览器不能提交任意 API path、method、effect 或 SubjectId；
- [ ] 能力批准校验当前 SubjectId 和 descriptor version；
- [x] Chat 撤销使用固定 POST 规则删除；阻止仍由 daccess 生命周期清理规则。
- [ ] 重试不会产生重复 grant、重复 rule 或重复通知。

### 用户界面

- [x] 请求中心不再显示“批准联系人”入口，联系人详情按 Chat 能力授予/撤销。
- [ ] 联系人详情显示双方能力状态和明确方向；
- [ ] 能力包含的多个 API 只显示为一个能力决定，API 仅在详情中说明；
- [ ] 待处理徽标覆盖能力请求和访问审批，并提供可访问文本语义。

## 14. 待确认决策

以下问题不阻塞模型落盘，但会影响 M3 之后的具体实现：

1. 是否支持“仅保存身份”的本地联系人记录。推荐支持，且明确标注为不含访问授权。
2. 是否在第一版就支持远端能力 descriptor 发现。推荐先完成本地提供能力的开放，远端能力请求作为 M5。
3. Chat 是否提供“双向 Chat”快捷预设。推荐提供，但内部仍保留两个方向的独立授权。
4. 能力申请是否允许已有联系人随时新增。推荐允许，且不重新触发联系人关系流程。
5. 能力版本升级是否需要重新授权。推荐需要，旧版本规则不自动扩展到新版本。

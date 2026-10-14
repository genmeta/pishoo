# std 服务与业务能力边界

日期：2026-10-14。状态：用户已要求按本文实施，第七节具体变更已纳入冻结接口。实现与验证进展见 IMPLEMENTATION.md；既有数据库、已发布制品和线上部署不自动迁移。旧 Workspace/Chat 系统 bundle 提案不恢复。

## 一、服务归属

`std` 是标准协议命名空间。是否内置、如何安装、由谁授权，分别表达不同事实。取消业务访问路径中的 `app` 和 `api` 前缀；包类型与 CLI 的 app/lib 安装操作继续保留。

| 服务 | 外部入口 | 交付与职责 | 数据归属 |
| --- | --- | --- | --- |
| ACL | `/std/acl` | 内置，daccess 负责规则、访问审批和授权结果 | 现有 daccess 数据库 |
| Contact | `/std/contact`，列表沿用 `/std/contacts` | 内置，daccess 负责联系人、身份绑定和申请资源 | 现有 daccess 数据库 |
| Package | `/std/sys/package` | 内置，Pishoo 负责验签、依赖、安装、更新、卸载与登记 | 身份的 `config.db` 和制品目录 |
| Workspace | `/std/workspace/` 页面；管理业务为 `/std/workspace/...` | 商店安装的 App/Lib，负责工作台、资料、收藏及联系人申请投递队列；展示 ACL 和 Message 资源 | Profile 私有目录 |
| Profile | `/std/profile`、`/std/profile/avatar` | 由商店安装的 Profile Lib 提供，不内置于宿主 | Profile 私有目录 |
| Message | `/std/message` 及其管理子路径 | 商店安装的独立 Message Lib，负责消息、会话、消息投递与对端权限观察 | Message 私有目录 |
| Calendar | `/std/calendar` | 商店安装的 Lib；当前仓库没有实现，本轮只确定协议归属 | 对应 Lib 私有目录 |

业务内置范围为 ACL、Contact，另有内置系统软件包管理。现有配置、文件服务与 DHTTP 转发是宿主基础设施，本轮不将它们改造成商店业务应用；其进一步路径归并不混入本次业务能力变更。

Workspace 不拥有联系人表、消息表或授权决定副本。Message 不负责联系人创建或 ACL 审批；消息格式、幂等、收件身份匹配仍由 Message 自身校验。Profile 可以继续与 Workspace 共用一个 Lib，不因标准入口不同增加一个后端包。

## 二、命名与路由

原 Chat Lib 改名为 Message：包短名和业务能力组标识均为 `message`，展示名称为“1v1聊天”。Workspace App 精确依赖 Profile 与 Message；Profile 无后端依赖；后端源码目录目标为 `std/libs/message`。已发布包须递增版本、重建并重新签名，不能覆盖已发布的同版本内容。

Lib 的 OpenAPI 路径统一作为 `/std` 下的业务路径，宿主只加 `/std`，不再加包名或 `/api/<name>`。例：Message 声明 `/message`，Workspace 声明 `/profile` 和 `/workspace/context`。guest 收到去掉 `/std` 后的同一业务路径，query 原样保留。包完整身份仍是 publisher/name，不由 HTTP 路径推断。

App 静态资源挂载 `/std/<name>/`；入口由包短名与既有 `entry="/"` 派生，不增加 mount 字段。Workspace App 入口为 `/std/workspace/#/`。App 和同名 Lib 可共存：实际静态 GET/HEAD 与 Lib 声明操作按方法和路径分派，不用 App 的整个前缀遮住业务接口。

| 原入口 | 目标入口 |
| --- | --- |
| `/app/workspace/` | `/std/workspace/` |
| `/api/workspace/context` | `/std/workspace/context` |
| `/api/workspace/settings/profile` | `/std/workspace/settings/profile` |
| `/api/workspace/profile` | `/std/profile` |
| `/api/workspace/profile/avatar` | `/std/profile/avatar` |
| `/api/chat/message` | `/std/message` |
| `/api/chat/chat/context` | `/std/message/context` |
| `/api/chat/chat/messages` | `/std/message/messages` |
| `/api/chat/chat/messages/retry` | `/std/message/messages/retry` |
| Workspace 的硬编码 `/capabilities` | 原生 `/std/acl/capabilities` |
| `/std/pishoo/{libs,apps,lib-check,app-check,store-install,loaded-libs,loaded-apps}` | `/std/sys/package/` 下保留同名管理资源 |

Message 的原能力 grant/deny/revoke 和 capability-requests 接口退出授权职责，工作台直接使用 daccess 现有联系人、规则与审批 API。消息可用状态可以保留为 `/std/message/capability` 的业务查询，但必须根据当前 ACL、联系人与对端观察计算。

同一个方法和路径只能有一个提供者；不同 Lib 不能瓜分同一个业务路径的不同方法。内置 ACL、Contact、系统管理及现有宿主基础设施路径不允许安装包占用。App 文件的 GET/HEAD 与 Lib 同路径操作冲突时拒绝安装，不做隐式优先覆盖。不同方法可以共享同名 App/Lib 的路径，例如 App GET `/std/workspace/` 和 Lib POST 同路径。

安装、更新和加载全部校验实际路由冲突；启动仍以登记制品为准，路由从已验签内容派生，不新增运行时路由注册状态或第三张路由表。不存在路径404，存在路径的方法未声明405；`/std` 内不回退普通代理。旧 `/app`、`/api` 不提供别名，走正常代理规则。

## 三、Lib 如何声明业务能力

复用 Lib OpenAPI 已有 `x-pishoo.access-groups` 和操作的 `x-pishoo.access-group`，不再增加一份 manifest 能力声明。包身份、版本和依赖来自已验签 WEBC manifest；业务能力与操作绑定来自同一验签包内的 OpenAPI。

```json
{
  "x-pishoo": {
    "access-groups": {
      "message": {"title": "1v1聊天", "operations": ["receiveMessage"]}
    }
  },
  "paths": {
    "/message": {
      "post": {
        "operationId": "receiveMessage",
        "summary": "接收联系人消息",
        "x-pishoo": {"access-group": "message"},
        "responses": {"200": {"description": "成功"}}
      }
    }
  }
}
```

此示例只是能力声明片段，完整文件继续满足现有 OpenAPI 校验。组内 operationId 必须存在，组和操作注解一致，操作不重复归组；沿用当前验证规则。

能力目录标识为 `<完整Lib ID>#<组标识>`，例如 `alice.smith.dhttp.net/message#message`。展示使用已验签的 `title`，不再依赖前端硬编码 `chat` 或 `public_profile` 列表。版本使用所属 Lib 包版本，不另设能力版本字段。App 不声明业务能力。

`package.pishoo.host` 继续描述 private-storage、request-identity、signatures 等宿主能力用途，与业务能力目录分开；声明和安装均不自动产生 Allow、Review 或 Deny 规则。

## 四、安装时登记能力

能力目录是已安装 Lib 的持久登记，放入身份的 `config.db`；授权与审批仍放在 daccess 数据库。两者不共享一份“批准状态”。建议在已有安装登记 schema 中增加以下两表：

```sql
CREATE TABLE lib_capabilities (
    lib_id TEXT NOT NULL REFERENCES libs(id) ON DELETE CASCADE,
    capability_id TEXT NOT NULL CHECK (length(capability_id) > 0),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    PRIMARY KEY (lib_id, capability_id)
);

CREATE TABLE lib_capability_operations (
    lib_id TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    method TEXT NOT NULL CHECK (length(method) > 0 AND method = upper(method)),
    path TEXT NOT NULL CHECK (substr(path, 1, 5) = '/std/'),
    PRIMARY KEY (lib_id, capability_id, method, path),
    UNIQUE (method, path),
    FOREIGN KEY (lib_id, capability_id)
        REFERENCES lib_capabilities(lib_id, capability_id) ON DELETE CASCADE
);
```

不存重复的包版本、发布者、能力计数、审批标志或加载状态，均从已有登记和实际资源取得。path 存完整外部业务路径，安装前执行已有规范化校验。SQL 的 UNIQUE 只约束已登记能力操作，其他 Lib 操作、静态文件与内置路由仍需按第二节核对。

普通安装、商店安装和首次普通发行安装复用同一流程：验签、核对业务能力、编译、检查依赖及路由冲突；在已有身份文件锁和安装事务中保存 libs、依赖与两张能力表，沿用现有文件发布与登记失败恢复机制。更新替换该 Lib 的目录项；无组写零行；卸载按外键移除目录并保留私有数据。

启动核对能力登记与对应已验签制品的一致性，不扫描文件反写目录、不隐式导入旧包。现有安装须通过正常重新安装登记能力；缺表或登记不一致时明确提示所需操作，不假装目录为空。数据库结构的兼容处理按第七节批准后实施。

目录更新不扩大既有 ACL 授权。现有 ACL 按路径匹配，Lib 改路径后的权限不能从旧路径推断。同路径授权也没有 Lib 身份绑定：卸载后若换发布者或换包来接管原操作，必须核对已有规则并经显式处理，不能仅因目录标识不同就声称旧权限已失效；不得用安装账本隐藏这个约束。

## 五、已有能力集合 API

新增原生 `GET /std/acl/capabilities`，在现有 Router 中装配处理器，直接只读 `config.db`，不增加 Server/Sandbox 字段或跨模块函数。返回安装能力集合，与访问者已获准使用的能力区分。

```json
[
  {
    "id": "alice.smith.dhttp.net/message#message",
    "title": "1v1聊天",
    "version": "0.3.0",
    "visibility": "contact",
    "approval_mode": "capability",
    "selectable": true,
    "endpoints": [{"method": "POST", "path": "/std/message"}]
  }
]
```

沿用前端 CapabilityDescriptor，仅新增 `title: string`。既有 visibility/approval_mode/selectable 对这类可申请业务能力分别为 contact/capability/true，仅表达目录类型，不能证明已授权或联系人已激活。version 从 libs 表取得；例中的新版本仅用于说明，不指定实际发布版本。响应按 id 和端点排序，Cache-Control 为 no-store；鉴权沿用 daccess，不在安装时自动开放目录。

工作台从本机目录展示可提供能力；向对端申请时读取对端目录、选择对端返回的能力标识，并转换为已有 requested_access 的 method/path。对端目录访问被拒绝或失败时报告未知，不能用本机已安装目录假定远端能力。能力发现 API 不返回任何联系人的批准状态、消息数据或宿主 imports。

## 六、ACL 审批使用能力名称

联系人 requested_access 和普通 access_reviews 沿用已有 method/path 数据。Workspace 审批列表和确认弹窗只展示能力目录中的名称（如“1v1聊天”），不展示 API 方法或路径；未匹配目录时显示“未登记能力”。调用原生 daccess API 仍使用实际 method/path，目录匹配同时核对方法，不仅匹配路径。

主人允许 Message 能力时，使用已有 `PATCH /std/acl/allow/{name}` 写入 POST `/std/message` 的 Allow；拒绝或撤销通过已有 Deny/移除规则语义执行。联系人激活仍是联系人资源的独立操作，不因联系人建立就自动允许消息。

`PATCH /std/acl/review` 的结果仍是既有请求审批结果，不代表永久写入 Allow；UI 必须区分“批准本次请求”和“允许该能力”。复用 daccess 实际提供的接口和语义，不恢复已废弃 HTTP 状态协议，不新增审批容器或后台等待器。

一个能力有多个端点时，当前 daccess set_policy 是逐条提交；工作台显示实际成功和失败，不宣称批量原子授权。当前“1v1聊天”只有一个入站操作，无需为此引入批量方法或新状态。

Message 收件通过宿主 daccess 后，仍检查真实 caller 与当前联系人、消息归属和幂等约束。删除“ACL 已允许，但 Message 私有 capability_decisions 再次决定是否允许”的第二授权来源。已有两张 capability_decisions/capability_decision_events 表不再创建、写入或参与判断；旧数据保留且不自动转成 ACL。

Message 私有数据仍保存消息、会话、消息队列和对端权限观察；后者是远端事实的缓存，不能当作本端授权。旧 chat 私有数据不因包改名自动迁移，升级迁移须单独明确。

## 七、冻结契约的具体差异与实施验证

本轮已明确的方向是业务路径统一 `/std`、内置范围、安装登记能力、ACL 使用能力名称，以及 `chat` 改为 `message`。用户已要求按本文实施，以下具体差异已列入冻结清单：

1. config.db 安装 schema 新增第四节完整定义的两张表；已有 libs/apps 与依赖字段不变，不修改 daccess schema。
2. `CapabilityDescriptor` 新增 `title: string`；`ChatCapabilityState.capability` 的字面量从 `chat` 改为 `message`；`CapabilityApproval.capability_id` 从 `chat` 改为 `string`，用来承载目录完整 ID。旧能力审批类型退出实际授权流程后可删除，但删除也须列入对应接口修订。
3. Message 新建库不再创建 capability_decisions/capability_decision_events；既有库允许保留两表，不读写、不导入。其他消息表暂保留现有字段，不能借改名调整状态结构。
4. Lib OpenAPI 路径改为标准服务路径；App/Lib 共用 `/std` 的具体分派、冲突规则及管理入口按第二节替换旧路由约定。Workspace/Profile 与 Message 包需重建新版本。

无需新增 Rust 有状态结构、成员、方法或跨模块函数；复用现有 Router、Sandbox/Lib、安装方法和 daccess API。源码目录、包名和依赖名改为 message，不增加 App 领域模型或宿主特殊权限。

实施后验证：包验签与 access-groups 一致性；普通/商店安装和能力登记失败恢复；重启核验与卸载级联；同名 App/Lib 共存和静态/业务冲突；无消息权限、批准、撤销与联系人身份变化；对端目录发现；旧路径代理及新路径404/405；工作台动态能力展示。本次验证结果及仍未完成的线上发布见 IMPLEMENTATION.md。

依据：`design/README.md`、`design/pishoo-interfaces.md`、`design/workspace-chat-interfaces.md`、现行包规范及本地 daccess 的 router/set_policy/decide_review 实现。

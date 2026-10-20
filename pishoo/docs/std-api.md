2026-10-20：包管理仅保留安装/更新/卸载和数据库已安装清单/详情读取。check、openapi、loaded 辅助接口已删除，未保留别名；校验在实际安装或更新中执行。

# Pishoo std HTTP API

本文列出 Pishoo 原生服务、Profile、Message、Calendar Lib 以及 Journal 示例当前提供的 HTTP 接口，供逐项调整接口使用。按服务分组，每个接口独立列出功能、请求示例、参数、响应示例及错误说明。

接口行为以当前工作区实现和 Cargo.lock 锁定的 daccess 0.1.1 为准。各接口以方法和路径定位。

## 1. 通用约定

### 1.1 路径与身份

原生服务由 Pishoo 提供。业务 Lib 的 OpenAPI 使用不带 `/std` 的字面路径，宿主对外添加 `/std`。Profile、Message、Calendar 和 Journal 的接口仅在对应 Lib 已安装并加载后存在。

App 的页面与静态资源挂载在 `/<name>/`，例如 `/workspace/`、`/calendar/`，不作为本清单中的业务 API。`/std` 根路径没有目录接口；`/std/file` 根路径返回 404。

名称和 SubjectId 来自经过验证的 DHTTP/mTLS 身份，普通 HTTP 请求头不能声明可信身份。“所有者”表示调用者名称和 SubjectId 都与当前服务身份相符。联系人、规则和审批管理的授权由 daccess 决定；部分 Pishoo 原生管理接口及 Lib 处理器另行要求所有者。

### 1.2 请求与成功响应

除 WEBC 上传、图片上传、文件下载和转发外，请求体与业务响应体使用 JSON。接口实际使用 HTTP/3。以下以便于阅读的文本形式展示方法、请求目标、头字段和消息体，不表示线上编码格式；省略 HTTP 版本，状态码后的原因短语仅用于阅读。Host 表示请求目标 authority。

示例中的 Host: bob.dhttp.net 是接收请求的服务身份，Accept: application/json 表示示例期望的表示格式；两者不用于声明调用者身份。参数表中的“必填”只说明当前接口输入契约，普通示例头不表示处理器会执行内容协商。路径和 query 示例值均为可替换的具体值，省略参数、null 和空值的语义单独说明。

200、201、202 均可能携带业务响应；204 不携带响应体。成功状态码按各接口当前实现分别列出，不将其统一改写为某种目标规范。

部分通过 Axum `get` 注册的原生路由会隐式接受 HEAD，但仍受 HEAD 对应的授权规则约束。清单只将明确实现的 HEAD 单列；Lib 不自动把 GET 声明扩展为 HEAD。

### 1.3 分页

使用页码分页的默认格式为：

```json
{
  "items": [],
  "total": 0,
  "page": 1,
  "page_size": 20
}
```

`page` 默认 1，必须大于 0；`page_size` 默认 20，范围 1 至 100。支持排序的接口使用 `order=asc|desc`，默认 desc。Message 使用游标分页，Calendar 使用时间范围查询，Journal 使用 offset 分页，分别见对应接口。

### 1.4 时间

联系人、本地联系人申请、Profile 和 Message 的整数时间戳使用 Unix 秒。Calendar 和 Journal 的整数时间戳使用 Unix 毫秒。审批的 `requested_at` 使用 Unix 秒，`expired_after` 使用 RFC 3339 字符串。

### 1.5 外层授权响应

普通接口在进入业务处理器前经过 daccess 授权。拒绝可返回 403；进入审批时返回：

```http
202 Accepted
Content-Type: application/json

{
  "review_id": 12,
  "status_url": "/std/acl/review/12/status"
}
```

此响应表示原业务尚未执行。调用者使用同一真实身份查询状态，allowed 后以相同请求字段重试原业务请求。它不同于 Message 发送接口表示消息已入队的 202。

`GET /std/contact/self` 和 `GET /std/acl/review/{id}/status` 由处理器按真实调用者校验记录归属，绕过普通业务 ACL，以免状态查询再次进入审批。

### 1.6 错误响应

错误格式按当前产生错误的层次区分。

| 来源 | 响应体 |
| --- | --- |
| daccess、原生配置/包管理、Lib 外层分派 | 主要为纯文本；部分仅有状态码 |
| 原生本地 Contact、Profile、Message 处理器 | `{"error":"错误说明"}` |
| Calendar 处理器 | `application/problem+json`，见“Calendar API” |
| Journal 处理器 | `{"error":"错误代码","message":"错误说明"}` |

输入、身份、资源不存在、冲突、大小和媒体类型错误常见为 400、403、404、409、413、415。未分类内部错误为 500，SQLite 忙时部分模块返回 503。具体行为在接口中补充；外层授权或分派错误不保证采用 Lib 自己的错误格式。

### 1.7 权限映射

联系人申请中的 `requested_access` 按路径列出请求的方法：

```json
{
  "/std/message": ["POST"]
}
```

`offers` 和 `granted_access` 按路径和 effect 列出方法：

```json
{
  "/std/message": {
    "allow": ["POST"],
    "review": [],
    "deny": []
  }
}
```

方法使用大写 HTTP 方法或 `*`。同一路径的方法不能跨 effect 重复。联系人交换的声明与本地 ACL 规则是不同事实；保存声明不等于写入授权。

## 2. 联系人 API

联系人资源由 daccess 管理 Router 提供。除另有说明外，管理请求经过统一 ACL，不额外将所有管理操作限定为所有者。

联系人收藏和出站申请由原生 Contact 服务提供，除统一 ACL 外，还核对调用者为当前身份所有者。响应携带 `Cache-Control: no-store`，错误体为 `{"error":"说明"}`。

收藏和出站申请在本章采用去掉 `local` 的简化路径；代码路由尚待同步。requests 表示当前身份发出的联系人申请。

### 2.1 `GET /std/contacts`

分页查询联系人。

**请求示例**

```http
GET /std/contacts?page=1&page_size=20
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | page | integer | 否 | 1 | 页码，必须大于 0 |
| 查询 | page_size | integer | 否 | 20 | 每页 1 至 100 条 |
| 查询 | sort | string | 否 | updated_at | name（兼容 names）、alias、updated_at、created_at、class |
| 查询 | order | string | 否 | desc | asc 或 desc |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "items": [
    {
      "id": 1,
      "incoming": true,
      "name": "alice.dhttp.net",
      "subject_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "alias": "Alice",
      "class": "Human",
      "description": "Alice",
      "status": "active",
      "created_at": 1792368000,
      "updated_at": 1792368000,
      "expired_after": 1792972800,
      "requested_access": {},
      "granted_access": {},
      "offers": {}
    }
  ],
  "total": 1,
  "page": 1,
  "page_size": 20
}
```

**说明与错误**

查询参数：`page`、`page_size`、`sort`、`order` 均可省略。`sort` 接受 `name`（兼容别名 `names`）、`alias`、`updated_at`、`created_at`、`class`，默认 `updated_at desc`。

`incoming` 仅列表返回，表示该名称与 SubjectId 是否存在已收到申请。`alias` 可以为 null。状态接受 `pending`、`active`、`transfered`、`expired`、`blocked`；`transfered` 是当前实际拼写。时间为 Unix 秒。

### 2.2 `DELETE /std/contacts`

批量删除联系人及其精确主体规则。

**请求示例**

```http
DELETE /std/contacts
Host: bob.dhttp.net
Content-Type: application/json

{
  "names": [
    "alice.dhttp.net",
    "carol.dhttp.net"
  ]
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | names | string[] | 是 | — | 要删除的联系人名称数组 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

`names` 必填，为名称数组。成功返回 204。操作在一个事务内执行；任一联系人不存在，或批次会移除某个 ACL 管理作用域的最后一位 allow 管理员时，整个批次失败。

### 2.3 `POST /std/contact`

以真实调用者身份提交联系人申请。

**请求示例**

```http
POST /std/contact
Host: bob.dhttp.net
Content-Type: application/json

{
  "application_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "class": "Human",
  "description": "Alice",
  "requested_access": {
    "/std/message": [
      "POST"
    ]
  },
  "offers": {
    "/std/message": {
      "allow": [
        "POST"
      ],
      "review": [],
      "deny": []
    }
  }
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | application_id | string | 是 | — | 64 位小写十六进制，重试保持不变 |
| 请求体 | description | string | 是 | — | 可为空字符串，最多 80 个 Unicode 字符 |
| 请求体 | class | string | 否 | 空字符串 | 联系人分类 |
| 请求体 | requested_access | object | 否 | {} | 按路径列出请求的方法 |
| 请求体 | offers | object | 否 | {} | 按路径及 effect 列出提供的方法 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
201 Created
Content-Type: application/json

{
  "application_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "name": "bob.dhttp.net",
  "subject_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "status": "pending",
  "received_at": 1792368000,
  "expired_after": 1792972800,
  "granted_access": {}
}
```

**说明与错误**

`application_id` 和 `description` 必填。编号为 64 位小写十六进制，在重试中保持不变。description 可为空，最多 80 个 Unicode 字符。class 默认空字符串，两个权限映射默认空对象。名称、SubjectId 和时间不能由请求体指定。

首次创建返回 201，相同身份、编号和载荷重试返回 200。

响应 name 和 subject_id 是接收方身份。首次申请有请求权限时为 pending，无请求权限时直接 active；有效期从实际收到时间起算 7 天。编号被其他身份或不同载荷复用时冲突。offers 不自动写入 ACL。

### 2.4 `GET /std/contact/self`

查询真实调用者自己的申请回执或当前联系人状态。

**请求示例**

```http
GET /std/contact/self
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | application_id | string | 否 | — | 64 位小写十六进制；省略时返回调用者当前联系人状态 |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "id": 1,
  "name": "alice.dhttp.net",
  "subject_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "status": "active",
  "granted_access": {
    "/std/message": {
      "allow": [
        "POST"
      ],
      "review": [],
      "deny": []
    }
  }
}
```

提供 application_id 时，请求与响应示例：

```http
GET /std/contact/self?application_id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Host: bob.dhttp.net
Accept: application/json
```

```http
200 OK
Content-Type: application/json

{
  "application_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "name": "bob.dhttp.net",
  "subject_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "status": "pending",
  "received_at": 1792368000,
  "expired_after": 1792972800,
  "granted_access": {}
}
```

**说明与错误**

可选查询参数 `application_id` 为 64 位小写十六进制。提供时成功返回 200，响应与 `POST /std/contact` 的申请回执相同；状态为 pending、active、expired、denied 或 revoked，name/subject_id 表示接收方。

不提供编号时成功返回 200。

此时 name/subject_id 表示真实调用者。active 时 granted_access 来自接收端当前精确 ACL；其他状态为空对象。未认证返回 400，资源不存在 404，归属或 SubjectId 不符 403。

### 2.5 `GET /std/contact/{name}`

读取指定联系人的完整资料和授权声明。

**请求示例**

```http
GET /std/contact/alice.dhttp.net
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | name | string | 是 | — | 联系人名称 |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "id": 1,
  "name": "alice.dhttp.net",
  "subject_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "alias": null,
  "class": "Human",
  "description": "Alice",
  "status": "active",
  "created_at": 1792368000,
  "updated_at": 1792368000,
  "expired_after": 1792972800,
  "requested_access": {},
  "granted_access": {},
  "offers": {}
}
```

**说明与错误**

成功返回 200，结构与 `GET /std/contacts` 的单个联系人相同，但不包含 incoming。

联系人不存在返回 404。

### 2.6 `PUT /std/contact/{name}`

所有者创建或重试本机联系人资源，不向远端发送申请。

**请求示例**

```http
PUT /std/contact/alice.dhttp.net
Host: bob.dhttp.net
Content-Type: application/json

{
  "subject_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "description": "Alice",
  "class": "Human",
  "status": "active",
  "requested_access": {},
  "offers": {},
  "expired_after": 1893456000
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | name | string | 是 | — | 联系人名称，不能为 owner 保留名 |
| 请求体 | subject_id | string | 是 | — | UTF-8 长度 1 至 64 字节，不执行十六进制解码 |
| 请求体 | description | string | 是 | — | 最多 80 字符，可为空 |
| 请求体 | class | string | 否 | 空字符串 | 联系人分类 |
| 请求体 | status | string | 否 | pending | pending 或 active |
| 请求体 | requested_access | object | 否 | {} | 权限请求声明 |
| 请求体 | offers | object | 否 | {} | 提供权限声明，不自动写 ACL |
| 请求体 | expired_after | integer | 否 | 新建为当前时间 + 7 天；已有沿用 | Unix 秒，必须晚于当前时间 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
201 Created
```

无响应体。

**说明与错误**

subject_id 和 description 必填。subject_id 按 UTF-8 字节保存，长度 1 至 64 字节，不做十六进制解码。description 最多 80 字符。class 默认空字符串；status 仅 pending/active，默认 pending；权限映射默认空对象。首次省略 expired_after 时使用当前时间加 7 天；已有资源省略时沿用已有到期时间，到期时间必须晚于当前时间。

首次创建返回 201，已有资源的相同内容重试成功返回 204，均无响应体。不允许替换的状态返回 409。不能写 owner 保留名称，也不能输入 id、incoming、granted_access 等派生字段。

### 2.7 `PATCH /std/contact/{name}`

管理者更新联系人状态或本地别名。

**请求示例**

```http
PATCH /std/contact/alice.dhttp.net
Host: bob.dhttp.net
Content-Type: application/json

{
  "status": "blocked",
  "alias": "Alice"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | name | string | 是 | — | 联系人名称 |
| 请求体 | status | string | 至少一项 | 保持原值 | active 或 blocked；与 alias 至少提供一项 |
| 请求体 | alias | string | 至少一项 | 保持原值 | 空字符串清除；与 status 至少提供一项 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

status 和 alias 可以分别省略，但至少提供一项。status 只允许 active 或 blocked；空 alias 清除别名。状态转换受联系人状态机约束，拉黑会保存并撤销该联系人的精确规则；取消拉黑复用库的恢复行为。

成功返回 204。空对象、未知字段、非法状态或破坏最后一位 ACL 管理员约束的更新失败。处理器要求已验证 Visitor，但可以是已授权的其他管理员。

### 2.8 `DELETE /std/contact/{name}`

删除指定联系人及其精确主体规则。

**请求示例**

```http
DELETE /std/contact/alice.dhttp.net
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | name | string | 是 | — | 联系人名称 |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

成功返回 204。不存在返回 404；删除会使 ACL 管理作用域失去最后一位 allow 管理员时返回 400。

### 2.9 `GET /std/contact/saved`

读取当前有效的收藏联系人名称。

**请求示例**

```http
GET /std/contact/saved
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json

[
  "alice.dhttp.net"
]
```

### 2.10 `PUT /std/contact/saved`

收藏一个已有联系人。

**请求示例**

```http
PUT /std/contact/saved?name=alice.dhttp.net
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | name | string | 是 | — | 目标联系人名称 |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

成功返回 204。收藏绑定当前联系人记录及 SubjectId；联系人不存在时失败。

### 2.11 `DELETE /std/contact/saved`

取消收藏指定联系人。

**请求示例**

```http
DELETE /std/contact/saved?name=alice.dhttp.net
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | name | string | 是 | — | 目标联系人名称 |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

取消收藏，成功返回 204；没有对应收藏也成功。

### 2.12 `GET /std/contact/requests`

分页查询本地出站申请，或按 id 查询单条申请。

**请求示例**

```http
GET /std/contact/requests?page=1&page_size=20
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | integer | 否 | — | 提供时查单条，省略时分页 |
| 查询 | page | integer | 否 | 1 | 页码，必须大于 0 |
| 查询 | page_size | integer | 否 | 20 | 每页 1 至 100 条 |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "items": [
    {
      "id": 1,
      "target_name": "alice.dhttp.net",
      "description": "Bob",
      "requested_capabilities": [
        "message"
      ],
      "offered_capabilities": [],
      "status": "queued",
      "expired_after": 1792972800,
      "delivery_deadline": 1792972800,
      "remote_expired_after": null,
      "last_checked_at": null,
      "error_message": null,
      "created_at": 1792368000,
      "updated_at": 1792368000
    }
  ],
  "total": 1,
  "page": 1,
  "page_size": 20
}
```

提供 id 时，请求与响应示例：

```http
GET /std/contact/requests?id=1
Host: bob.dhttp.net
Accept: application/json
```

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "id": 1,
  "target_name": "alice.dhttp.net",
  "description": "Bob",
  "requested_capabilities": [
    "message"
  ],
  "offered_capabilities": [],
  "status": "queued",
  "expired_after": 1792972800,
  "delivery_deadline": 1792972800,
  "remote_expired_after": null,
  "last_checked_at": null,
  "error_message": null,
  "created_at": 1792368000,
  "updated_at": 1792368000
}
```

**说明与错误**

提供查询参数 id 时返回一条记录；不提供时接受 page/page_size，默认 1/20、最大 100，按 id 倒序分页。无请求体。

单条查询成功返回 200。

分页返回 `{"items":[上述记录],"total":1,"page":1,"page_size":20}`。时间为 Unix 秒。状态包括 queued、pending、active、denied、expired、revoked、failed。id 不存在返回 404。

### 2.13 `POST /std/contact/requests`

创建出站联系人申请，由后台投递和查询远端回执。

**请求示例**

```http
POST /std/contact/requests
Host: bob.dhttp.net
Content-Type: application/json

{
  "target_name": "alice.dhttp.net",
  "description": "Bob",
  "requested_capabilities": [
    "message"
  ],
  "offered_capabilities": []
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | target_name | string | 是 | — | 目标身份，不能是自己 |
| 请求体 | description | string | 是 | — | 最多 80 字符，不接受控制字符 |
| 请求体 | requested_capabilities | string[] | 是 | — | 对方目录中的权限组名称，不重复 |
| 请求体 | offered_capabilities | string[] | 是 | — | 本机目录中的权限组名称，不重复 |
| 请求头 | Content-Type | string | 否 | 不校验该头 | 请求体按 JSON 解析，示例使用 application/json |

**响应示例**

```http
201 Created
Content-Type: application/json

{
  "id": 1,
  "target_name": "alice.dhttp.net",
  "description": "Bob",
  "requested_capabilities": [
    "message"
  ],
  "offered_capabilities": [],
  "status": "queued",
  "expired_after": 1792972800,
  "delivery_deadline": 1792972800,
  "remote_expired_after": null,
  "last_checked_at": null,
  "error_message": null,
  "created_at": 1792368000,
  "updated_at": 1792368000
}
```

**说明与错误**

四个字段均必填。description 最多 80 字符，不接受控制字符；两个能力数组存放权限组名称，不能重复，必须存在于相应一方能力目录。不能向自己申请，请求体最大 64 KiB。

成功返回 201，响应为 `GET /std/contact/requests` 的单条记录。相同目标已有 queued/pending 申请时返回 409。若 offered_capabilities 非空，处理器还会核验远端身份并写入相应本地授权；创建操作可能发起远端请求。

### 2.14 `DELETE /std/contact/requests`

删除本地出站申请记录。

**请求示例**

```http
DELETE /std/contact/requests?id=1
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | integer | 是 | — | 本地记录 id |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

成功返回 204。记录不存在或仍被在途作业领取时返回 409。此操作删除本地记录，不向远端发送撤销通知。

### 2.15 `POST /std/contact/requests/refresh`

提前安排指定出站申请的下一次处理。

**请求示例**

```http
POST /std/contact/requests/refresh?id=1
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | integer | 是 | — | 本地记录 id |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "id": 1,
  "target_name": "alice.dhttp.net",
  "description": "Bob",
  "requested_capabilities": [
    "message"
  ],
  "offered_capabilities": [],
  "status": "queued",
  "expired_after": 1792972800,
  "delivery_deadline": 1792972800,
  "remote_expired_after": null,
  "last_checked_at": null,
  "error_message": null,
  "created_at": 1792368000,
  "updated_at": 1792368000
}
```

**说明与错误**

将 queued/pending/active 记录的下一次处理时间提前。成功返回 200，响应为 `GET /std/contact/requests` 的单条记录，不等待本次远端刷新完成；id 不存在返回 404。

## 3. ACL 与审批 API

除能力目录由 Pishoo 原生提供外，本节使用当前 daccess 管理 Router。method 为大写 HTTP 方法或 `*`；effect 为 allow/review/deny。

grantee 可为具体身份、`组名@签发者`、`**`（具名）、`?`（匿名）、`*?`（全部）。修改规则须保留 ACL 管理作用域至少一位精确身份或组的 allow 管理员。

### 3.1 `GET /std/acl/promissions`

读取已登记 Lib 的业务能力目录。

**请求示例**

```http
GET /std/acl/promissions
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "read message": {
    "/std/message": ["GET", "HEAD"],
    "/std/message/history": ["GET"]
  }
}
```

**说明与错误**

顶层键为已登记 Lib 的 access-group title，值为路径到大写 HTTP 方法数组的映射。同名组按路径合并，方法排序去重；空目录为 `{}`。不返回 id、版本、visibility、approval_mode、selectable 或 endpoints 元数据。联系人申请中的 requested_capabilities/offered_capabilities 使用这些权限组名称。响应带 no-store。新身份默认允许匿名 GET，已有身份仍以自己的 ACL 为准；目录登记或安装不自动授权。

### 3.2 `GET /std/acl/access`

按 API、方法、effect 查询全部显式规则。

**请求示例**

```http
GET /std/acl/access
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "/std/message": {
    "POST": {
      "allow": [
        "alice.dhttp.net"
      ],
      "review": [],
      "deny": []
    }
  }
}
```

### 3.3 `GET /std/acl/access/{api}`

查询指定 API 的各方法授权规则。

**请求示例**

```http
GET /std/acl/access/std/message
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | api | string | 是 | — | 完整 API 路径，省略开头 /；可包含多个路径段 |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "POST": {
    "allow": [
      "alice.dhttp.net"
    ],
    "review": [],
    "deny": []
  }
}
```

### 3.4 `POST /std/acl/access/{api}`

创建或覆盖指定 API 的一条授权规则。

**请求示例**

```http
POST /std/acl/access/std/message
Host: bob.dhttp.net
Content-Type: application/json

{
  "method": "POST",
  "effect": "allow",
  "grantee": "alice.dhttp.net"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | api | string | 是 | — | 完整 API 路径省略开头 / |
| 请求体 | method | string | 是 | — | 大写 HTTP 方法或 * |
| 请求体 | effect | string | 是 | — | allow、review 或 deny |
| 请求体 | grantee | string | 是 | — | 身份、组或特殊选择器 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

三项必填，请求体不携带 api。成功返回 204。除 owner 外，精确身份必须已在联系人表中。设置 `*?` 会删除同一 method/API 的 `**` 和 `?`；反向设置同理。

### 3.5 `GET /std/acl/apis`

分页列出存在显式规则的 API。

**请求示例**

```http
GET /std/acl/apis?page=1&page_size=20
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | page | integer | 否 | 1 | 页码，必须大于 0 |
| 查询 | page_size | integer | 否 | 20 | 每页 1 至 100 条 |
| 查询 | sort | string | 否 | updated_at | api 或 updated_at |
| 查询 | order | string | 否 | desc | asc 或 desc |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "items": [
    {
      "api": "/std/message",
      "updated_at": 1792368000
    }
  ],
  "total": 1,
  "page": 1,
  "page_size": 20
}
```

**说明与错误**

分页单位是存在显式规则的不同 API；不表示全部已安装业务操作。

### 3.6 `GET /std/acl/allow`

按 grantee、API、effect 查询全部显式规则。

**请求示例**

```http
GET /std/acl/allow
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "alice.dhttp.net": {
    "/std/message": {
      "allow": [
        "POST"
      ],
      "review": [],
      "deny": []
    }
  }
}
```

### 3.7 `GET /std/acl/allow/{name}`

按 API 分页查询指定 grantee 的显式规则。

**请求示例**

```http
GET /std/acl/allow/alice.dhttp.net?page=1&page_size=20
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | name | string | 是 | — | 规则 grantee |
| 查询 | page | integer | 否 | 1 | 页码，必须大于 0 |
| 查询 | page_size | integer | 否 | 20 | 每页 1 至 100 条 |
| 查询 | sort | string | 否 | updated_at | api 或 updated_at |
| 查询 | order | string | 否 | desc | asc 或 desc |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "items": {
    "/std/message": {
      "POST": {
        "allow": [
          "alice.dhttp.net"
        ],
        "review": [],
        "deny": []
      }
    }
  },
  "total": 1,
  "page": 1,
  "page_size": 20
}
```

**说明与错误**

items 为对象，结构与 `GET /std/acl/access` 相同；分页单位和 total 都是该 grantee 关联的不同 API 数量。

### 3.8 `PATCH /std/acl/allow/{name}`

创建或覆盖指定 grantee 的一条授权规则。

**请求示例**

```http
PATCH /std/acl/allow/alice.dhttp.net
Host: bob.dhttp.net
Content-Type: application/json

{
  "method": "POST",
  "api": "/std/message",
  "effect": "review"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | name | string | 是 | — | 规则 grantee |
| 请求体 | method | string | 是 | — | 大写 HTTP 方法或 * |
| 请求体 | api | string | 是 | — | 以 / 开头的 API 路径 |
| 请求体 | effect | string | 是 | — | allow、review 或 deny |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

三项必填。effect 可为 allow、review、deny，并非仅设置 allow。成功返回 204。

### 3.9 `DELETE /std/acl/allow/{name}`

删除指定 grantee 的一条授权规则。

**请求示例**

```http
DELETE /std/acl/allow/alice.dhttp.net
Host: bob.dhttp.net
Content-Type: application/json

{
  "method": "POST",
  "api": "/std/message"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | name | string | 是 | — | 规则 grantee |
| 请求体 | method | string | 是 | — | 大写 HTTP 方法或 * |
| 请求体 | api | string | 是 | — | 以 / 开头的 API 路径 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

两项必填，不携带 effect。成功返回 204，不存在也按幂等删除成功；不能移除最后一位 ACL 管理员。

### 3.10 `GET /std/acl/reviews`

分页查询待审批或已过期的未决审批记录。

**请求示例**

```http
GET /std/acl/reviews?status=pending&page=1&page_size=20
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | status | string | 否 | pending | pending 或 expired |
| 查询 | page | integer | 否 | 1 | 页码，必须大于 0 |
| 查询 | page_size | integer | 否 | 20 | 每页 1 至 100 条 |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "items": [
    {
      "id": 12,
      "requested_at": 1792368000,
      "visitor": "alice.dhttp.net",
      "method": "POST",
      "api": "/std/message",
      "reason": "matched review rule",
      "expired_after": "2026-10-20T12:00:00Z"
    }
  ],
  "total": 1,
  "page": 1,
  "page_size": 20
}
```

**说明与错误**

两种列表都只选尚未决定的记录，以到期时间区分 pending/expired。requested_at 为 Unix 秒，expired_after 为 RFC 3339。

### 3.11 `PATCH /std/acl/review`

对未过期的待审批记录作出决定。

**请求示例**

```http
PATCH /std/acl/review
Host: bob.dhttp.net
Content-Type: application/json

{
  "id": 12,
  "action": "allow",
  "expired_after": "2030-01-01T00:00:00Z"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | id | integer | 是 | — | 审批 id |
| 请求体 | action | string | 是 | — | allow 或 deny |
| 请求体 | expired_after | string | 是 | — | 未来时间，RFC 3339 字符串 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

三项必填；action 为 allow 或 deny；expired_after 为晚于当前时间的 RFC 3339 字符串。成功返回 204；不存在、已决定或已过期的 id 返回 404。不创建长期 ACL 规则，决定由原业务请求重试消费。

### 3.12 `DELETE /std/acl/review/{id}`

删除已过期且仍未决定的审批记录。

**请求示例**

```http
DELETE /std/acl/review/12
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | id | integer | 是 | — | 已过期的未决审批 id |

**响应示例**

```http
204 No Content
```

无响应体。

**说明与错误**

成功返回 204；不存在或条件不满足返回 404。

### 3.13 `GET /std/acl/review/{id}/status`

原申请者查询自己的审批状态。

**请求示例**

```http
GET /std/acl/review/12/status
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | id | integer | 是 | — | 审批 id |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "status": "pending"
}
```

**说明与错误**

status 为 pending、allowed、denied、expired。不存在或身份不匹配返回 404，缺少已验证身份返回 403。查询不消费决定。

## 4. 系统配置 API

所有接口要求当前身份所有者。写入 JSON 要求 `Content-Type: application/json`，可带 charset，最大 64 KiB。GET/DELETE 不接受非空请求体。响应携带 `Cache-Control: no-store`、`Supported-Versions: v1`。

可选请求头 `Accept-Versions` 接受包含 v1 的逗号列表和多个同名头；省略使用 v1，当前无匹配版本返回 505。不支持的方法返回 405 并带 Allow；无效输入 400、身份不符 403、超大写请求 413、非 JSON 写请求 415、配置冲突 409、存储失败 500。

### 4.1 `GET /std/sys/settings`

读取数据库中保存的期望设置。

**请求示例**

```http
GET /std/sys/settings
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求头 | Accept-Versions | string | 否 | — | 可省略；省略使用 v1，提供时须包含 v1 |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store
Supported-Versions: v1

{
  "enabled": true,
  "listen": 3
}
```

**说明与错误**

enabled 为布尔值，控制整个身份 Server 是否加载。listen 为整数 0、1、2、3，分别表示不监听、内网、外网、两者。

### 4.2 `PATCH /std/sys/settings`

部分更新身份启用状态和监听范围。

**请求示例**

```http
PATCH /std/sys/settings
Host: bob.dhttp.net
Content-Type: application/json

{
  "enabled": true,
  "listen": 1
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | enabled | boolean | 至少一项 | 保持原值 | 是否加载整个身份 Server；与 listen 至少提供一项 |
| 请求体 | listen | integer | 至少一项 | 保持原值 | 0/1/2/3：不监听/内网/外网/两者 |
| 请求头 | Accept-Versions | string | 否 | — | 可省略；省略使用 v1，提供时须包含 v1 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store
Supported-Versions: v1

{
  "enabled": true,
  "listen": 1
}
```

**说明与错误**

两个字段均可省略，但至少提供一项；不接受其他字段。成功返回 200，响应为保存后的完整设置。

成功表示期望配置已保存，运行进程随后扫描应用，不承诺响应时监听已切换。禁用身份后，其 HTTP 管理入口也会不可达。

### 4.3 `GET /std/sys/proxies`

读取全部代理规则，或按 location 查询单条规则。

**请求示例**

```http
GET /std/sys/proxies
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | location | string | 否 | — | 规则唯一键；省略返回数组，提供返回单条 |
| 请求头 | Accept-Versions | string | 否 | — | 可省略；省略使用 v1，提供时须包含 v1 |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store
Supported-Versions: v1

[
  {
    "location": "/service/",
    "proxy_pass": "http://127.0.0.1:8080/"
  }
]
```

提供 location 时，请求与响应示例：

```http
GET /std/sys/proxies?location=%2Fservice%2F
Host: bob.dhttp.net
Accept: application/json
```

```http
200 OK
Content-Type: application/json
Cache-Control: no-store
Supported-Versions: v1

{
  "location": "/service/",
  "proxy_pass": "http://127.0.0.1:8080/"
}
```

**说明与错误**

不带 location 时成功返回 200。

提供 location 时返回单个对象。

指定规则不存在返回 404。查询参数只能为恰好一次 location。

### 4.4 `PUT /std/sys/proxies`

原子替换整个代理规则列表。

**请求示例**

```http
PUT /std/sys/proxies
Host: bob.dhttp.net
Content-Type: application/json

[
  {
    "location": "/service/",
    "proxy_pass": "http://127.0.0.1:8080/"
  }
]
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | 整个请求体 | object[] | 是 | — | 代理规则数组，空数组清空全部 |
| 请求体 | [].location | string | 是 | — | 规则唯一键 |
| 请求体 | [].proxy_pass | string | 是 | — | 回环 HTTP/TCP 上游地址 |
| 请求头 | Accept-Versions | string | 否 | — | 可省略；省略使用 v1，提供时须包含 v1 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store
Supported-Versions: v1

[
  {
    "location": "/service/",
    "proxy_pass": "http://127.0.0.1:8080/"
  }
]
```

**说明与错误**

每条仅接受两个字符串字段。空数组清空全部规则。location 是唯一键；`= /path` 表示精确匹配，其他按路径段前缀匹配。上游为现有回环 HTTP/TCP 地址；裸地址可规范化为 HTTP。不得占用 `/std`，也不能与已安装 App 挂载范围冲突。

成功返回 200，响应为保存后的完整数组，结构同请求。规则在既有 SQLite 事务中一起校验和提交。

### 4.5 `PATCH /std/sys/proxies`

按 location 新增或替换单条代理规则。

**请求示例**

```http
PATCH /std/sys/proxies
Host: bob.dhttp.net
Content-Type: application/json

{
  "location": "/service/",
  "proxy_pass": "http://127.0.0.1:8081/"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | location | string | 是 | — | 规则唯一键；= /path 为精确匹配，其他为路径段前缀 |
| 请求体 | proxy_pass | string | 是 | — | 回环 HTTP/TCP 上游地址 |
| 请求头 | Accept-Versions | string | 否 | — | 可省略；省略使用 v1，提供时须包含 v1 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store
Supported-Versions: v1

{
  "location": "/service/",
  "proxy_pass": "http://127.0.0.1:8081/"
}
```

**说明与错误**

两项必填，校验规则同 `PUT /std/sys/proxies`。成功返回 200。

### 4.6 `DELETE /std/sys/proxies`

按 location 删除一条代理规则。

**请求示例**

```http
DELETE /std/sys/proxies?location=%2Fservice%2F
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | location | string | 是 | — | 规则唯一键，必须恰好出现一次 |
| 请求头 | Accept-Versions | string | 否 | — | 可省略；省略使用 v1，提供时须包含 v1 |

**响应示例**

```http
204 No Content
Cache-Control: no-store
Supported-Versions: v1
```

无响应体。

**说明与错误**

成功返回 204，不存在也成功。省略 location 不会清空列表，而是拒绝请求。

## 5. 软件包管理 API

完整请求、响应、状态码、数据库及失败边界统一维护在[软件包管理审查稿](package-management.md#4-http-api)，源包格式见[发布包规范](package-spec.md)。本章只保留导航。

| 路径 | 方法 | 当前行为 |
| --- | --- | --- |
| `/std/sys/lib/list`、`/std/sys/app/list` | GET | 读取数据库已安装数组 |
| `/std/sys/lib?name=NAME`、`/std/sys/app?name=NAME` | GET | 读取数据库详情 |
| `/std/sys/lib?name=NAME`、`/std/sys/app?name=NAME` | PUT | 从商店安装或更新精确版本；Content-Type 为 application/json，body 恰好为 `{"publisher":"alice.dhttp.net","version":"1.2.3"}` |
| `/std/sys/lib?name=NAME`、`/std/sys/app?name=NAME` | DELETE | 卸载，保留业务数据、ACL 和依赖包 |

name 只在 URI 中；PUT JSON 不接收 name、kind、URL 或本地源包。安装与更新复用同一 PUT，保持单包 upsert 行为；POST 返回 405，原始 WEBC PUT 返回 415。不提供本地安装、check/openapi/loaded 等入口，也不提供安装账本或自动重试。

校验发生在实际商店安装/更新时。安装登记各七列：name、author、version、title、description、license、repository，不保存本地源摘要、作者指纹或防篡改字段。列表/详情不是运行健康或文件完整性检查。同类名称跨作者唯一，App/Lib 名称空间独立；作者只在元数据中。新业务数据使用 `db/<name>/`，兼容使用已有 `db/<author>/<name>/`，不自动迁移或新建作者目录。

独立商店尚不接受新 Lib 的内嵌 OpenAPI 格式，因此不能宣称已有可绕过商店的安装路径；兼容阻塞及其他生命周期边界见审查稿。

## 6. Profile API

读取仍受普通 ACL 约束；GET profile/avatar/context 及 HEAD context 允许非所有者进入处理器，新身份有相应公开默认规则。修改要求真实所有者，并拒绝 Sec-Fetch-Site 为 cross-site 的写操作。除上下文使用 private, no-store 外，处理器默认 no-store。

### 6.1 `GET /std/profile`

读取公开身份资料。

**请求示例**

```http
GET /std/profile
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "identity_name": "bob.dhttp.net",
  "subject_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "display_name": "Bob",
  "avatar_url": "/std/profile/avatar",
  "updated_at": 1792368000
}
```

**说明与错误**

display_name/avatar_url 可为 null，updated_at 为 Unix 秒。

### 6.2 `PATCH /std/profile`

所有者修改展示名称。

**请求示例**

```http
PATCH /std/profile
Host: bob.dhttp.net
Content-Type: application/json

{
  "display_name": "Bob"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | display_name | string | 是 | — | 去首尾空白，最多 80 字符，空字符串清空 |
| 请求头 | Content-Type | string | 否 | 不校验该头 | 请求体按 JSON 解析，示例使用 application/json |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "identity_name": "bob.dhttp.net",
  "subject_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "display_name": "Bob",
  "avatar_url": "/std/profile/avatar",
  "updated_at": 1792368000
}
```

**说明与错误**

display_name 必填，去首尾空白，最多 80 字符，不允许控制字符；空字符串清空，请求体不接受其他字段。JSON 请求体最大 64 KiB。

成功返回 200，响应为 `GET /std/profile` 的完整 Profile。

### 6.3 `GET /std/profile/avatar`

读取当前头像，支持按 ETag 条件查询。

**请求示例**

```http
GET /std/profile/avatar
Host: bob.dhttp.net
If-None-Match: "旧头像内容摘要.png"
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求头 | If-None-Match | string | 否 | — | 与当前 ETag 完全相等时返回 304 |

**响应示例**

```http
200 OK
Content-Type: image/png
ETag: "头像内容摘要.png"
Cache-Control: no-store
```

响应体：PNG 图片二进制；也可能为 JPEG 或 WebP，示例省略内容。

ETag 匹配时：

```http
304 Not Modified
ETag: "头像内容摘要.png"
Cache-Control: no-store
```

**说明与错误**

成功返回 200 图片二进制，Content-Type 为 image/jpeg、image/png 或 image/webp，并带 ETag。请求头值与当前 ETag 完全相等时返回 304，无响应体；无头像返回 404。

### 6.4 `PUT /std/profile/avatar`

所有者替换头像。

**请求示例**

```http
PUT /std/profile/avatar
Host: bob.dhttp.net
Content-Type: image/png
```

请求体：PNG 图片二进制；示例省略内容。

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求头 | Content-Type | string | 是 | — | image/jpeg、image/png 或 image/webp，与实际内容一致 |
| 请求体 | 整个请求体 | binary | 是 | — | 图片最大 1 MiB，宽高各 1 至 2048 |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "identity_name": "bob.dhttp.net",
  "subject_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "display_name": "Bob",
  "avatar_url": "/std/profile/avatar",
  "updated_at": 1792368000
}
```

**说明与错误**

支持 image/jpeg、image/png、image/webp，媒体类型必须匹配实际内容。图片最大 1 MiB，宽高分别为 1 至 2048。

成功返回 200，响应为 `GET /std/profile` 的完整 Profile，包含更新后的 avatar_url/updated_at。格式不支持返回 415，尺寸无效返回 400，体积超限返回 413。

### 6.5 `DELETE /std/profile/avatar`

所有者清除当前头像关联。

**请求示例**

```http
DELETE /std/profile/avatar
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "identity_name": "bob.dhttp.net",
  "subject_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "display_name": "Bob",
  "avatar_url": null,
  "updated_at": 1792368000
}
```

**说明与错误**

成功返回 200，响应为 `GET /std/profile` 的完整 Profile，其中 avatar_url 为 null。

### 6.6 `GET /std/profile/context`

读取公开名片及真实访客上下文。

**请求示例**

```http
GET /std/profile/context
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: private, no-store

{
  "identity_name": "bob.dhttp.net",
  "display_name": "Bob",
  "avatar_url": "/std/profile/avatar",
  "updated_at": 1792368000,
  "viewer_name": "alice.dhttp.net",
  "is_owner": false
}
```

**说明与错误**

display_name/avatar_url 可为 null。匿名 viewer_name 为 null，is_owner 同时核对真实名称与主体。响应不包含 subject_id，携带 `Cache-Control: private, no-store`。

### 6.7 `HEAD /std/profile/context`

读取名片上下文的响应头，不返回响应体。

**请求示例**

```http
HEAD /std/profile/context
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: private, no-store
```

无响应体。

## 7. Message API

除接收入口 POST /std/message 外，全部要求真实所有者。写操作拒绝 cross-site。处理器响应默认 no-store，错误格式为 `{"error":"说明"}`。

### 7.1 `GET /std/message/context`

读取消息模块上下文。

**请求示例**

```http
GET /std/message/context
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "owner_name": "bob.dhttp.net",
  "capability": "message",
  "endpoints": [
    {
      "method": "POST",
      "path": "/std/message"
    }
  ]
}
```

### 7.2 `GET /std/message/capability`

查询与指定联系人的当前收发能力状态。

**请求示例**

```http
GET /std/message/capability?name=alice.dhttp.net
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | name | string | 是 | — | 目标联系人名称 |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "capability": "message",
  "status": "available",
  "contact_status": "active",
  "can_send": true,
  "can_receive": true,
  "remote_grant": true,
  "endpoints": [
    {
      "method": "POST",
      "path": "/std/message"
    }
  ]
}
```

**说明与错误**

status 为 blocked、available 或 waiting，remote_grant 为 true、false 或 null。can_receive 来自本地当前 ACL，can_send 取决于 active 联系人及远端授权观察。尚无观察时可能同步请求远端并保存观察结果。

### 7.3 `GET /std/message/messages`

按游标读取与指定联系人的消息。

**请求示例**

```http
GET /std/message/messages?name=alice.dhttp.net&after=0&limit=50
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | name | string | 是 | — | 目标联系人名称 |
| 查询 | after | integer | 否 | 0 | 上一页 next_cursor，按消息 id 升序 |
| 查询 | limit | integer | 否 | 50 | 1 至 100 |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "items": [
    {
      "id": "1",
      "client_message_id": "m_123",
      "remote_message_id": null,
      "direction": "outgoing",
      "state": "queued",
      "sender": "bob.dhttp.net",
      "recipient": "alice.dhttp.net",
      "text": "你好",
      "created_at": 1792368000,
      "updated_at": 1792368000,
      "error_message": null
    }
  ],
  "next_cursor": null
}
```

**说明与错误**

有后续页时 next_cursor 为本页最后 id 的字符串，下一页通过 after 传入。direction 为 incoming/outgoing；state 包括 queued、sending、sent、received、failed、blocked。时间为 Unix 秒。

### 7.4 `POST /std/message/messages`

创建本地出站消息并排队投递。

**请求示例**

```http
POST /std/message/messages?name=alice.dhttp.net
Host: bob.dhttp.net
Content-Type: application/json

{
  "text": "你好"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | name | string | 是 | — | 目标联系人名称 |
| 请求体 | text | string | 是 | — | 去首尾空白后非空，最多 4000 字符；控制字符只允许换行、制表符 |
| 请求头 | Content-Type | string | 否 | 不校验该头 | 请求体按 JSON 解析，示例使用 application/json |

**响应示例**

```http
202 Accepted
Content-Type: application/json
Cache-Control: no-store

{
  "id": "1",
  "client_message_id": "m_123",
  "remote_message_id": null,
  "direction": "outgoing",
  "state": "queued",
  "sender": "bob.dhttp.net",
  "recipient": "alice.dhttp.net",
  "text": "你好",
  "created_at": 1792368000,
  "updated_at": 1792368000,
  "error_message": null
}
```

**说明与错误**

text 必填，去首尾空白后不能为空，最多 4000 字符；控制字符只允许换行和制表符。JSON 请求体最大 64 KiB。client_message_id 由服务端生成，不接受客户端传入。

成功返回 202，响应为 `GET /std/message/messages` items 中的一条完整消息，state 初始为 queued。此时消息已在本地创建，不表示远端已收到。联系人不满足发送条件返回 403。

### 7.5 `POST /std/message/messages/retry`

将失败或被阻止的出站消息重新排队。

**请求示例**

```http
POST /std/message/messages/retry?name=alice.dhttp.net&id=1
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | name | string | 是 | — | 目标联系人名称 |
| 查询 | id | integer | 是 | — | 本地记录 id |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "id": "1",
  "client_message_id": "m_123",
  "remote_message_id": null,
  "direction": "outgoing",
  "state": "queued",
  "sender": "bob.dhttp.net",
  "recipient": "alice.dhttp.net",
  "text": "你好",
  "created_at": 1792368000,
  "updated_at": 1792368000,
  "error_message": null
}
```

**说明与错误**

只重新排队 outgoing 且 state 为 failed/blocked 的消息。成功返回 200，响应为 `GET /std/message/messages` 的单条消息，state 变为 queued。不可重试返回 409，无发送权限返回 403。

### 7.6 `POST /std/message`

接收真实 active 联系人的消息。

**请求示例**

```http
POST /std/message
Host: bob.dhttp.net
Content-Type: application/json

{
  "client_message_id": "m_123",
  "text": "你好"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | client_message_id | string | 是 | — | 1 至 128 字节，ASCII 字母/数字/-/_，重试保持不变 |
| 请求体 | text | string | 是 | — | 去首尾空白后非空，最多 4000 字符；控制字符只允许换行、制表符 |
| 请求头 | Content-Type | string | 否 | 不校验该头 | 请求体按 JSON 解析，示例使用 application/json |

**响应示例**

```http
201 Created
Content-Type: application/json
Cache-Control: no-store

{
  "id": "1",
  "client_message_id": "m_123",
  "sender": "alice.dhttp.net",
  "recipient": "bob.dhttp.net",
  "text": "你好",
  "created_at": 1792368000
}
```

**说明与错误**

两项必填。client_message_id 为 1 至 128 字节，仅接受 ASCII 字母、数字、`-`、`_`；text 校验同 `POST /std/message/messages`。发送人来自真实身份，不能由 JSON 指定。

首次创建返回 201，相同联系人、client_message_id 和正文重试返回 200。

相同编号重用不同正文返回 409；身份、联系人状态或权限不符返回 403。

## 8. Calendar API

所有接口均要求真实所有者，并经过普通 ACL；拒绝 cross-site 修改。时间统一为 Unix 毫秒。JSON 写入要求 application/json，请求体最大 64 KiB。处理器错误使用 application/problem+json，响应携带 private, no-store：

```http
409 Conflict
Content-Type: application/problem+json
Cache-Control: private, no-store

{
  "type": "about:blank",
  "title": "Conflict",
  "status": 409,
  "detail": "The event has changed. Reload it before editing."
}
```

### 8.1 `GET /std/calendar/events`

查询与指定时间范围相交的日程。

**请求示例**

```http
GET /std/calendar/events?from=1792368000000&to=1792454400000
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | from | integer | 是 | — | Unix 毫秒，非负 |
| 查询 | to | integer | 是 | — | Unix 毫秒，大于 from，范围不超过 370 天 |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: private, no-store

{
  "events": [
    {
      "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "title": "会议",
      "description": "讨论安排",
      "location": "办公室",
      "start_at": 1792400400000,
      "end_at": 1792404000000,
      "all_day": false,
      "color": "blue",
      "created_at": 1792368000000,
      "updated_at": 1792368000000
    }
  ]
}
```

**说明与错误**

要求 to > from，范围不超过 370 天。查询与半开时间范围相交的事件，即 start_at < to 且 end_at > from。按 start_at、id 升序。

最多返回 10000 条；超出返回 413，要求缩小范围，当前无分页。无效范围或未知查询参数返回 400。

### 8.2 `POST /std/calendar/events`

创建日程，由服务端生成 id。

**请求示例**

```http
POST /std/calendar/events
Host: bob.dhttp.net
Content-Type: application/json

{
  "title": "会议",
  "description": "讨论安排",
  "location": "办公室",
  "start_at": 1792400400000,
  "end_at": 1792404000000,
  "all_day": false,
  "color": "blue"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | title | string | 是 | — | 去首尾空白后非空，最多 120 字符 |
| 请求体 | description | string | 是 | — | 最多 8000 字符，可空 |
| 请求体 | location | string | 是 | — | 最多 240 字符，可空 |
| 请求体 | start_at | integer | 是 | — | Unix 毫秒，0 至 253402300799999 |
| 请求体 | end_at | integer | 是 | — | Unix 毫秒，大于 start_at，范围同 start_at |
| 请求体 | all_day | boolean | 是 | — | 全天事件起止必须为 UTC 午夜 |
| 请求体 | color | string | 是 | — | blue、green、yellow、purple 或 pink |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
201 Created
Content-Type: application/json
Cache-Control: private, no-store

{
  "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "title": "会议",
  "description": "讨论安排",
  "location": "办公室",
  "start_at": 1792400400000,
  "end_at": 1792404000000,
  "all_day": false,
  "color": "blue",
  "created_at": 1792368000000,
  "updated_at": 1792368000000
}
```

**说明与错误**

七个字段均必填，不接受其他字段。title 去首尾空白后非空且最多 120 字符；description 最多 8000 字符；location 最多 240 字符，后二者可为空。字符串会去首尾空白；description 允许换行、回车、制表符。

start_at/end_at 范围为 0 至 253402300799999，end_at > start_at；全天事件起止必须为 UTC 午夜。color 仅 blue、green、yellow、purple、pink。

成功返回 201，响应为 `GET /std/calendar/events` events 中的单个完整事件。id 由服务端生成，为 32 位小写十六进制；created_at/updated_at 为 Unix 毫秒。

### 8.3 `GET /std/calendar/event`

读取单个日程。

**请求示例**

```http
GET /std/calendar/event?id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | string | 是 | — | 32 位小写十六进制 id |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: private, no-store

{
  "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "title": "会议",
  "description": "讨论安排",
  "location": "办公室",
  "start_at": 1792400400000,
  "end_at": 1792404000000,
  "all_day": false,
  "color": "blue",
  "created_at": 1792368000000,
  "updated_at": 1792368000000
}
```

**说明与错误**

成功返回 200，响应为 `GET /std/calendar/events` 的单个完整事件；不存在返回 404。

### 8.4 `PUT /std/calendar/event`

按原 updated_at 版本全量替换日程。

**请求示例**

```http
PUT /std/calendar/event?id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Host: bob.dhttp.net
Content-Type: application/json

{
  "title": "会议",
  "description": "调整时间",
  "location": "办公室",
  "start_at": 1792404000000,
  "end_at": 1792407600000,
  "all_day": false,
  "color": "blue",
  "updated_at": 1792368000000
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | string | 是 | — | 32 位小写十六进制 id |
| 请求体 | title | string | 是 | — | 去首尾空白后非空，最多 120 字符 |
| 请求体 | description | string | 是 | — | 最多 8000 字符，可空 |
| 请求体 | location | string | 是 | — | 最多 240 字符，可空 |
| 请求体 | start_at | integer | 是 | — | Unix 毫秒，0 至 253402300799999 |
| 请求体 | end_at | integer | 是 | — | Unix 毫秒，大于 start_at，范围同 start_at |
| 请求体 | all_day | boolean | 是 | — | 全天事件起止必须为 UTC 午夜 |
| 请求体 | color | string | 是 | — | blue、green、yellow、purple 或 pink |
| 请求体 | updated_at | integer | 是 | — | 读取到的原版本，Unix 毫秒 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: private, no-store

{
  "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "title": "会议",
  "description": "调整时间",
  "location": "办公室",
  "start_at": 1792404000000,
  "end_at": 1792407600000,
  "all_day": false,
  "color": "blue",
  "created_at": 1792368000000,
  "updated_at": 1792368000001
}
```

**说明与错误**

七个内容字段的校验同 `POST /std/calendar/events`；另须提交读取到的原 updated_at 作为版本。成功返回 200，响应为更新后的完整事件，updated_at 至少比原值增加 1。不存在返回 404，版本冲突 409。

### 8.5 `DELETE /std/calendar/event`

按原 updated_at 版本删除日程。

**请求示例**

```http
DELETE /std/calendar/event?id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&updated_at=1792368000000
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | string | 是 | — | 32 位小写十六进制 id |
| 查询 | updated_at | integer | 是 | — | 读取到的原版本，Unix 毫秒 |

**响应示例**

```http
204 No Content
Cache-Control: private, no-store
```

无响应体。

**说明与错误**

id 为 32 位小写十六进制，updated_at 为读取到的原版本。成功返回 204，不存在 404，版本冲突 409。

## 9. 文件与 DHTTP 转发 API

### 9.1 `GET /std/file/{path}`

读取当前身份 file 目录下的文件。

**请求示例**

```http
GET /std/file/example.txt
Host: bob.dhttp.net
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | path | string | 是 | — | file 目录相对路径，可含多段 |

**响应示例**

```http
200 OK
Content-Type: text/plain
Content-Length: 5

hello
```

**说明与错误**

成功返回 200 文件二进制，附带按文件扩展名推导的 Content-Type 和实际 Content-Length。路径指向目录时尝试 index.html；无文件返回 404。

路径做百分号解码，拒绝点路径段、反斜杠、NUL 等无效输入；文件访问受统一 ACL。`/std/file` 根路径自身返回 404。

### 9.2 `HEAD /std/file/{path}`

读取文件响应头，不返回文件内容。

**请求示例**

```http
HEAD /std/file/example.txt
Host: bob.dhttp.net
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | path | string | 是 | — | file 目录相对路径，可含多段 |

**响应示例**

```http
200 OK
Content-Type: text/plain
Content-Length: 5
```

无响应体。

### 9.3 任意方法 `/std/dhttp/{target}[/{path}]`

所有者使用当前身份凭据向目标 DHTTP 身份转发请求。

**请求示例**

```http
GET /std/dhttp/alice.dhttp.net/std/profile
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 路径 | target | string | 是 | — | 目标身份，可带 :证书序号 |
| 路径 | path | string | 否 | / | 目标路径，可包含多段 |
| 查询 | 目标 query | 依目标接口 | 依目标接口 | — | 原样转发 |
| 请求头 | 普通头字段 | 依目标接口 | 依目标接口 | — | 清理逐跳头，Host 改为目标 |
| 请求体 | 整个请求体 | 依目标接口 | 依目标接口 | — | 流式转发 |

**响应示例**

```http
200 OK
Content-Type: application/json

{
  "identity_name": "alice.dhttp.net",
  "subject_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "display_name": "Alice",
  "avatar_url": "/std/profile/avatar",
  "updated_at": 1792368000
}
```

**说明与错误**

target 为单个路径段中的目标身份，可带 `:证书序号`；path 为目标路径，可省略。目标根地址有无末尾斜杠都支持。query、原 HTTP 方法、普通请求头和 Body 继续转发。

返回远端状态、普通响应头和流式 Body；无统一 JSON 响应模型。清理逐跳头，Host 改为目标，不转发入站可信身份 extensions。远端看到当前服务身份。

除普通 ACL 外，处理器复核调用者为当前身份所有者。无效目标或不支持的请求 trailers 返回 400，身份不符 403，出站失败 502。该入口不接受本地 HTTP/TCP 目标，不回退到普通反向代理。

## 10. Journal 示例 API

Journal 位于 examples，不属于标准三个 Lib；安装并加载后采用相同 `/std` 分派。所有操作要求真实具名身份和普通 ACL；除 POST comments 允许具名访客外，修改还要求所有者。

全部写操作要求 `X-Journal-Write: 1`，拒绝 cross-site 及不匹配 Origin。JSON 写入要求 application/json，最大 1.5 MiB。id 均为 32 位小写十六进制；新内容 id 由客户端预先生成。时间为 Unix 毫秒，响应携带 private, no-store。

### 10.1 `GET /std/journal/context`

读取 Journal 的真实调用者和所有者上下文。

**请求示例**

```http
GET /std/journal/context
Host: bob.dhttp.net
Accept: application/json
```

**参数**

无业务路径参数、查询参数或请求体。通用请求头见“请求与成功响应”。

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: private, no-store

{
  "owner_name": "bob.dhttp.net",
  "caller_name": "alice.dhttp.net",
  "is_owner": false
}
```

### 10.2 `GET /std/journal/entries`

按 offset 分页查询手记摘要。

**请求示例**

```http
GET /std/journal/entries?offset=0
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | offset | integer | 否 | 0 | 0 至 100000，每页 20 篇 |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: private, no-store

{
  "entries": [
    {
      "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "title": "今天",
      "preview": "记录今天的事情",
      "created_at": 1792368000000,
      "updated_at": 1792368000000,
      "version": "1",
      "has_photo": false,
      "comment_count": 0
    }
  ],
  "next_offset": null
}
```

**说明与错误**

preview 为正文前 180 个字符。有更多记录时 next_offset 为 offset + 20，否则 null。

### 10.3 `POST /std/journal/entries`

所有者新建手记。

**请求示例**

```http
POST /std/journal/entries
Host: bob.dhttp.net
Content-Type: application/json
X-Journal-Write: 1

{
  "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "title": "今天",
  "body": "记录今天的事情",
  "photo": null
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | id | string | 是 | — | 客户端生成的 32 位小写十六进制 |
| 请求体 | title | string | 是 | — | 最多 200 字符 |
| 请求体 | body | string | 是 | — | 非空白，UTF-8 最多 64 KiB |
| 请求体 | photo | string 或 null | 否 | 新建无图；编辑保留原图 | JPEG data URL，解码后最多 1 MiB；null 移除 |
| 请求头 | X-Journal-Write | string | 是 | — | 固定值 1 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
201 Created
Content-Type: application/json
ETag: "1"
Cache-Control: private, no-store

{
  "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "title": "今天",
  "body": "记录今天的事情",
  "created_at": 1792368000000,
  "updated_at": 1792368000000,
  "version": "1",
  "has_photo": false
}
```

**说明与错误**

id/title/body 必填，photo 可省略。title 最多 200 字符；body 非空白且 UTF-8 最多 64 KiB。photo 为 null 或 `data:image/jpeg;base64,...` 数据 URL，解码后最多 1 MiB。

成功返回 201，附带 `ETag: "1"`。

重复 id 返回 409，不覆盖已存在内容。

### 10.4 `GET /std/journal/entry`

读取完整手记及当前 ETag。

**请求示例**

```http
GET /std/journal/entry?id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | string | 是 | — | 32 位小写十六进制 id |

**响应示例**

```http
200 OK
Content-Type: application/json
ETag: "1"
Cache-Control: private, no-store

{
  "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "title": "今天",
  "body": "记录今天的事情",
  "created_at": 1792368000000,
  "updated_at": 1792368000000,
  "version": "1",
  "has_photo": false
}
```

**说明与错误**

成功返回 200，响应为 `POST /std/journal/entries` 的完整手记对象，附带对应 `ETag: "版本号"`；不存在返回 404。

### 10.5 `PUT /std/journal/entry`

所有者使用 If-Match 编辑手记。

**请求示例**

```http
PUT /std/journal/entry?id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Host: bob.dhttp.net
Content-Type: application/json
If-Match: "1"
X-Journal-Write: 1

{
  "title": "今天",
  "body": "更新后的正文",
  "photo": null
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | string | 是 | — | 32 位小写十六进制 id |
| 请求体 | title | string | 是 | — | 最多 200 字符 |
| 请求体 | body | string | 是 | — | 非空白，UTF-8 最多 64 KiB |
| 请求体 | photo | string 或 null | 否 | 新建无图；编辑保留原图 | JPEG data URL，解码后最多 1 MiB；null 移除 |
| 请求头 | If-Match | string | 是 | — | 单个带双引号的正整数强版本标签 |
| 请求头 | X-Journal-Write | string | 是 | — | 固定值 1 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
200 OK
Content-Type: application/json
ETag: "2"
Cache-Control: private, no-store

{
  "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "title": "今天",
  "body": "更新后的正文",
  "created_at": 1792368000000,
  "updated_at": 1792368000001,
  "version": "2",
  "has_photo": false
}
```

**说明与错误**

title/body 必填，限制同 `POST /std/journal/entries`；photo 省略保留原图，null 移除，数据 URL 替换。成功返回 200，响应为完整更新对象，附带递增版本 ETag。

缺少 If-Match 返回 428；要求单个带双引号的正整数强版本，不接受弱标签、通配符和列表；格式错误 400，旧版本 412。

### 10.6 `DELETE /std/journal/entry`

所有者使用 If-Match 删除手记及其留言。

**请求示例**

```http
DELETE /std/journal/entry?id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Host: bob.dhttp.net
Accept: application/json
If-Match: "1"
X-Journal-Write: 1
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | string | 是 | — | 32 位小写十六进制 id |
| 请求头 | If-Match | string | 是 | — | 单个带双引号的正整数强版本标签 |
| 请求头 | X-Journal-Write | string | 是 | — | 固定值 1 |

**响应示例**

```http
204 No Content
Cache-Control: private, no-store
```

无响应体。

**说明与错误**

成功返回 204。删除正文、照片及关联留言，保留手记 ID 墓碑，防止旧创建请求复活。版本校验及错误同 `PUT /std/journal/entry`。

### 10.7 `GET /std/journal/photo`

读取手记照片。

**请求示例**

```http
GET /std/journal/photo?id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Host: bob.dhttp.net
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | string | 是 | — | 手记 id，32 位小写十六进制 |

**响应示例**

```http
200 OK
Content-Type: image/jpeg
Cache-Control: private, no-store
```

响应体：JPEG 图片二进制；示例省略内容。

**说明与错误**

成功返回 200、Content-Type 为 image/jpeg，Body 为照片二进制；手记或照片不存在返回 404。

### 10.8 `GET /std/journal/comments`

读取指定手记的全部留言和一层回复。

**请求示例**

```http
GET /std/journal/comments?entry_id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Host: bob.dhttp.net
Accept: application/json
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | entry_id | string | 是 | — | 手记 id，32 位小写十六进制 |

**响应示例**

```http
200 OK
Content-Type: application/json
Cache-Control: private, no-store

{
  "comments": [
    {
      "id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "parent_id": null,
      "author": "alice.dhttp.net",
      "body": "你好",
      "created_at": 1792368000000,
      "deleted": false
    }
  ]
}
```

**说明与错误**

每篇最多 500 条，无分页。已删除留言 author/body 为 null、deleted 为 true，仍保留位置；手记不存在返回 404。

### 10.9 `POST /std/journal/comments`

具名访客发表留言或一层回复。

**请求示例**

```http
POST /std/journal/comments
Host: bob.dhttp.net
Content-Type: application/json
X-Journal-Write: 1

{
  "id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "entry_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "parent_id": null,
  "body": "你好"
}
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 请求体 | id | string | 是 | — | 客户端生成的 32 位小写十六进制 |
| 请求体 | entry_id | string | 是 | — | 手记 id，32 位小写十六进制 |
| 请求体 | parent_id | string 或 null | 否 | null | 同篇未删除的顶层留言 id；null 表示顶层留言 |
| 请求体 | body | string | 是 | — | 非空白，UTF-8 最多 8 KiB |
| 请求头 | X-Journal-Write | string | 是 | — | 固定值 1 |
| 请求头 | Content-Type | string | 是 | — | application/json |

**响应示例**

```http
201 Created
Content-Type: application/json
Cache-Control: private, no-store

{
  "id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "entry_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "parent_id": null,
  "author": "alice.dhttp.net",
  "body": "你好",
  "created_at": 1792368000000,
  "deleted": false
}
```

**说明与错误**

id/entry_id/body 必填。parent_id 可省略或为 null，指定时须指向同篇未删除的顶层留言。body 非空白，UTF-8 最多 8 KiB。author 从真实身份取得，不接受客户端指定。

id 重复或每篇已达到 500 条时返回 409，无效回复目标 400，手记不存在 404。

### 10.10 `DELETE /std/journal/comment`

所有者删除留言并保留墓碑。

**请求示例**

```http
DELETE /std/journal/comment?id=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
Host: bob.dhttp.net
Accept: application/json
X-Journal-Write: 1
```

**参数**

| 位置 | 参数 | 类型 | 必填 | 省略时 | 说明 |
| --- | --- | --- | --- | --- | --- |
| 查询 | id | string | 是 | — | 留言 id，32 位小写十六进制 |
| 请求头 | X-Journal-Write | string | 是 | — | 固定值 1 |

**响应示例**

```http
204 No Content
Cache-Control: private, no-store
```

无响应体。

**说明与错误**

成功返回 204。清空作者和正文，保留留言墓碑；不存在或已删除返回 404。

## 11. 实现位置

| 接口范围 | 实现 |
| --- | --- |
| 联系人、规则和审批 | Cargo.lock 锁定的 daccess 0.1.1，`src/api.rs` 及 `src/api/` |
| 本地联系人 | [contact.rs](../src/contact.rs) |
| 能力目录 | [server.rs](../src/server.rs) 的 native_router |
| 配置 | [setup.rs](../src/setup.rs) |
| 制品 | [package.rs](../src/package.rs) 及 [package](../src/package/) |
| Profile | [profile.rs](../../std/libs/profile/src/profile.rs) |
| Message | [message.rs](../../std/libs/message/src/message.rs) |
| Calendar | [calendar.rs](../../std/libs/calendar/src/calendar.rs) |
| 文件和转发 | [routes.rs](../src/routes.rs)、[outbound.rs](../src/routes/outbound.rs) |
| Journal 示例 | [journal.rs](../examples/journal/src/journal.rs) |

本清单描述现有行为。后续逐项调整时，HTTP 行为、调用方与文档须对应更新；涉及冻结结构、成员或跨模块签名的变更仍按 [架构契约](../../design/README.md) 执行。

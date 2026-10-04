# Pishoo 配置 API

配置接口由现有 dhttp Endpoint 的 H3 监听提供，参考 minidocs 的系统 API 命名与版本协商，以及当前 daccess 管理 API。每个身份操作自己的 `db/config.db`，继续使用 schema v1。

## 资源和方法

| 路径 | 方法 | 行为 | 成功响应 |
| --- | --- | --- | --- |
| `/sys/settings` | GET | 读取数据库中的 listen、exec | 设置对象 |
| `/sys/settings` | PATCH | 更新指定设置，至少提供一个字段 | 保存后的设置对象 |
| `/sys/proxies` | GET | 读取数据库中的代理规则 | 规则数组，按 location 排序 |
| `/sys/proxies` | PUT | 原子替换完整代理规则列表，空数组清除全部规则 | 保存后的规则数组 |

设置对象示例：

```json
{"listen": 3, "exec": false}
```

`listen` 仅接受整数 0、1、2、3，依次表示关闭、内网、外网、两者。`exec` 使用 JSON 布尔值，数据库中仍是整数 0/1。PATCH 省略的字段保持原值；并发更新通过 SQLite 即时事务串行执行，避免丢失其他字段的更新。

代理规则数组示例：

```json
[
  {"location": "/service/", "proxy_pass": "http://127.0.0.1:8080/"}
]
```

每条规则必须且只能包含字符串字段 `location`、`proxy_pass`。沿用已有的路径、重复规则和回环 HTTP/TCP 上游校验。`location` 是唯一键，不增加数据库 ID。`= /path` 表示精确匹配，其余位置按现有路径段前缀规则匹配。`/sys` 与其他管理、Lib API、静态文件命名空间保留，不能被代理或 Lib 占用。

裸上游地址规范化为 `http://` URI。未写路径的 `http://127.0.0.1:8080` 保留原请求路径；显式路径 `/` 或 `/api/` 替换匹配前缀，两者不会在读写中合并。

写请求必须使用 `Content-Type: application/json`，可带 charset 参数。请求体最多 64 KiB。未知字段、null、错误类型、无效代理与空 PATCH 均拒绝。完整输入校验后再开始写事务，数据库失败也不会留下部分代理规则。

## 权限和版本

请求经过现有 daccess 授权层；处理器还核对 middleware 从 TLS 构造的 Visitor 与 Endpoint 同名、owner_hash 相同。即使 ACL 放行，匿名、其他身份及同名不同 owner_hash 的来访者仍不能操作配置。请求头不能声明可信身份。联系人、ACL 和审批继续使用已挂载的 daccess API。

本版实施的是身份限制，未限制请求来自本地网络。minidocs 的系统 API 另有仅本地网络访问的约定，当前 `HandshakeSummary` 不包含网络范围，身份核对不能保证该约定。

支持 `Accept-Versions: v1`，也支持包含 v1 的逗号分隔列表和多个同名头。省略时使用 v1，没有匹配版本返回 505。配置处理器的响应携带 `Supported-Versions: v1` 和 `Cache-Control: no-store`。

成功返回 200 和 JSON；无效输入返回 400，身份不符返回 403，不支持的方法返回 405 并带 Allow，超大请求返回 413，非 JSON 写请求返回 415，存储错误返回 500，版本不匹配返回 505。外层 daccess 的拒绝与审批响应遵循当前库定义。

## 生效规则与 H3 调用

成功写入表示数据库已保存。GET 展示数据库配置；正在运行的配置不会自动更新。代理变更通过现有 SIGHUP 串行重载生效，listen/exec 变更需重启；只要数据库中的 listen/exec 与当前运行值不同，现有 reload 就会拒绝。

使用原生 H3 示例客户端；设置 DHTTP_HOME 指向客户端身份目录，并以目标身份的同名凭据连接：

```sh
export PISHOO_CLIENT_IDENTITY=alice.dhttp.net
cargo run --locked -p pishoo --example pishoo-client -- get /sys/settings
cargo run --locked -p pishoo --example pishoo-client -- patch /sys/settings '{"exec":true}'
cargo run --locked -p pishoo --example pishoo-client -- put /sys/proxies '[{"location":"/service/","proxy_pass":"http://127.0.0.1:8080/"}]'
cargo run --locked -p pishoo --example pishoo-client -- get /sys/proxies
```

接口随 Server 启动及重载装配；仅新增已批准的 `setup::config_router(profile, endpoint) -> Router` 跨模块函数。复用 ServerConfig、ProxyLocation 和现有资源；没有新增配置 DTO、Server 字段、数据库表或传输接口。

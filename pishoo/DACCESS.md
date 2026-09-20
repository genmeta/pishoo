# daccess 同进程集成

profile server 使用 daccess 作为唯一的访问控制实现。它不是 HTTP sidecar：DHTTP
listener、Axum management router、业务 `NginxRouter` 和授权 middleware 都在同一个
pishoo service 中。

## 运行时边界

- 仅 `ServerIdentity::Profile` 启用 daccess；直接证书 server 保留原有的
  `LocationRuleEvaluator`。
- 每个 profile 加载自己的 `db/access.db` 和 `AccessService`。启动时会创建父目录
  和 daccess schema。
- pishoo 从已完成 mTLS 验证的 DHTTP connection 中取得 peer name 与证书 SKI owner
  hash，构造 `Visitor`。普通 HTTP header 不参与身份构造。
- profile owner 保持默认 allow，其余请求默认 deny；`/contact`、`/contacts`、`/acl/*`
  和业务路由使用同一个授权 middleware。
- 命中 review 的业务请求立即返回 `202 Accepted`，正文给出 `review_id` 与
  `status_url`。没有 live review、请求 Future 或连接取消分支；连接关闭不会删除
  SQLite 中的 pending record。
- 调用方以同一已验证身份轮询 `status_url`。状态为 `allowed` 后重试相同业务请求，
  该一次性 allow/deny 决定才会被消费。状态查询绕过常规业务 ACL，但 handler 按
  `Visitor` 对 record 做行级校验。
- 联系人批准立即写入本地数据库。申请方以已验证身份调用 `GET /contact/self` 拉取
  自己的状态和当前授权；pishoo 不创建联系人 notifier、出站 connector 或重试队列。

## Workspace

管理界面属于 pishoo，而不是 daccess crate。Solid/Vite 源码位于
`pishoo/workspace/`，pishoo 的 Cargo build script 按 `bun.lock` 安装依赖并生成
`workspace/dist`，随后由 `WorkspaceAssets` 内嵌：

- `/workspace` 临时重定向到 `/workspace/`；
- `/workspace/` 返回 Workspace shell；
- `/workspace/access/<section>` 回退到 `index.html`，支持浏览器直接刷新深链；
- 带 hash 的 `/workspace/assets/*` 返回长期缓存响应；
- `/workspace-api/context` 返回当前 profile 的显示上下文；
- `/admin` 和 `/admin/*` 不提供兼容路由或重定向；
- 非 Workspace 路径仍委托给 daccess API 或原有 `NginxRouter`。

开发 Workspace：

```sh
cd pishoo/workspace
bun install --frozen-lockfile
PISHOO_WORKSPACE_BACKEND=http://127.0.0.1:3000 bun run dev
```

前端继续调用 daccess 的 `/acl/*`、`/contacts` 和 `/contact/*` API；这些路径不因
Workspace 迁移而改变，也继续使用同一个 profile-scoped 授权 middleware。

## 迁移旧策略

旧 pishoo profile server 使用 `dhttp-access` 的 `access_rules` schema；daccess 不会
读取或转换其中的规则。现有数据库可以保留旧表并新增 daccess schema，但旧规则不会
继续生效。

1. 备份旧 profile 的 `db/access.db`。
2. 将旧规则按 daccess API 重新创建；daccess 会在同一 `db/access.db` 中初始化它
   自己的 schema，而不会覆盖旧表。
3. 移除 profile server 的 `access_rules` directive 后重启 pishoo。

profile server 仍配置 `access_rules` 时会明确拒绝启动，避免静默忽略旧规则。直接
证书 server 不受此迁移影响。

## 开发依赖

当前工作区通过 `pishoo/Cargo.toml` 的相对 path dependency 引用相邻
`daccess/daccess` crate。因此 pishoo 和 daccess 必须作为同一 release checkout
提供；发布为独立源码包时，应将该 path dependency 替换为相同版本的 registry 或
git dependency。构建 daccess 本身不需要 Bun；构建 pishoo Workspace 需要 Bun 可从
`PATH` 找到。

# 普通用户 Lib 的宿主能力

Workspace 与其他用户应用使用相同的 App/Lib 机制。前端为独立零 atom WEBC，声明固定版本后端依赖；后端为一个 WASM component。宿主不再内嵌 Workspace 页面、资料、联系人队列或聊天模型。

`pishoo:identity/request-identity@0.2.0` 的两个 getter：

```wit
local: func() -> tuple<string, string>;
caller: func() -> option<tuple<string, string>>;
```

tuple 是规范化名称与证书 SKI owner_hash。主人比较完整 tuple；匿名 HTTP 与 CLI 后台调用的 caller 为 none，无效具名 SKI trap。旧 @0.1.0 与 signatures 保留兼容。

普通已安装 Lib 使用标准 WASI HTTP outgoing，以当前 Endpoint 发送远端 DHTTP 请求；本端目标通过原生 Router 分发，HTTP 继承真实 caller、CLI 后台使用实际 LocalAuthority。每次都经 daccess 授权，WASM 不能注入 Visitor 或身份事实。本端转交只替换认证 extensions，保留 Router 自己产生的路径参数。没有 Workspace 名称判断、系统 Linker或新增 HTTP WIT。

出站接受 HTTPS DHTTP authority，`~` 表示本端。保持正常证书验证、逐跳头清理、流式请求响应；正文各限1 MiB，总传输最多15秒，并遵循更短 WASI 期限。请求 trailers 拒绝。业务准入、申请状态、重试和队列全部在 Lib 中。

宿主每3秒驱动当前用户 Lib 中具有标准 wasi:cli/run 导出的组件。每次独立 Store、私有 filesystem、既有 fuel/limiter 与 TaskTracker；15秒超时取消并回收。一个运行批次未结束时不积累定时批次；HTTP 执行仍没有额外并发名额。没有 Server 业务 worker 字段。

原生资源 API 增强：联系人列表直接返回 id/incoming；详情和自身状态返回 id；PUT /std/contact/{name} 为 owner 专用本机资源写入。GET /std/acl/reviews 支持 pending/expired 与 requested_at；DELETE /std/acl/review/{id} 仅删除已过期 pending 记录。数据库 schema、授权模型和公共 ContactRecord/ReviewRecord 不变。

普通入口为 `/std/<业务路径>` 和 `/std/<name>/`，前端 hash 导航、后端字面量路径/query。通用已加载目录位于 `/std/sys/package/loaded-libs`、`/std/sys/package/loaded-apps`。

宿主发行包不携带业务 App/Lib，首次初始化不安装或下载默认制品。Pishoo 和提供 `genmeta package install` 的新版 gmutils 安装顺序不限；配置好身份、启动该身份的 Pishoo 服务后，由身份所属账户执行 `genmeta package install std.dhttp.net/workspace --id NAME`。宿主从商店下载、验签并递归安装精确后端依赖，在线即时生效；所需版本须已发布。卸载和依赖保护同其他 App/Lib，安装不授予额外权限。构建与独立发布制品详见 [Workspace 前端](../../std/apps/workspace/README.md)。

安装登记存入身份的 config.db：libs 与 apps 分开，各自 name 唯一；app_dependencies 保存完整后端 ID 和精确版本。签名制品保留在原有 publisher/name 目录。启动只加载数据库登记项，核对制品身份、版本、依赖；不扫描或自动导入其他文件。旧安装使用正常安装命令显式登记。

卸载 App 只移除前端制品、App 登记和该 App 的依赖关联，保留后端制品、Lib 登记和数据；被其他 App 依赖的 Lib 不能卸载，显式卸载 Lib 也保留数据。文件与数据库在已有身份锁下提交；进程意外中断导致两者不一致时明确报错，通过重新安装修复，不以文件反写数据库。

App 与 Lib 均可声明多个精确后端 dependencies（publisher/name/version，最多100；Lib省略时为[]）。config.db 的 app_dependencies 与 lib_dependencies 分别保存关联。宿主拒绝循环和不匹配版本，启动核对签名制品与登记；依赖被使用时不能卸载或修改其内容/版本。正常移除父制品只删除其关联，保留依赖制品和数据。商店安装递归补齐并按依赖顺序提交，一次最多解析100个Lib，父安装失败不撤销已成功安装的有效依赖。

Lib 间调用通过标准 WASI HTTP 向 https://~/std/<业务路径> 发请求。前台保留当前请求的真实调用者，继续受原生daccess和被调用Lib自身身份检查约束；后台CLI发起真实具名HTTP请求，调用身份由握手给出，CLI自身仍无caller。未声明依赖不会赋予额外权限，dependencies用于安装和生命周期约束。

前端详情展示所依赖后端及版本，静态文件GET/HEAD仅用于机器分派，不作为前端业务API展示。

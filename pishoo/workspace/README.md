# Workspace 普通用户前端

此目录是独立 App，挂载 `/app/workspace/`，使用相对静态资源和 hash 导航。包内没有 atom、OpenAPI 或逐文件静态操作；宿主从实际 public 文件树派生 GET/HEAD 路由，所有文件仍经过 daccess。

`wasmer.toml` 的 `package.pishoo.dependencies` 精确声明同一发布者的 `workspace@0.2.0` 和 `chat@0.2.0`。Workspace 后端负责资料与联系人申请队列，Chat 后端负责聊天；原生联系人、ACL、配置和制品管理仍调用 `/std` API。应用商店安装 App 会自动补装缺失后端，复用相同版本并拒绝冲突；本地文件安装要求后端已经安装。

构建使用支持现有 bun.lock 的 Bun（本次验证为 1.4.3）：

```sh
sh build.sh --publisher YOUR_NAME.dhttp.net
```

生成三份独立签名 WEBC，需要本机发布者凭据及 WASI SDK：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh package.sh --publisher YOUR_NAME.dhttp.net
```

输出为 `target/workspace-dist/default-artifacts/{chat-lib,workspace-lib,workspace-app}.webc`，不上传商店或修改用户数据。设置 `PISHOO_PACKAGE_WEBC` 可以选择已构建打包器。构建输入生成在 `package-dist/`，不进入 public 文件树；源文件不会被发布者参数改写。

应用列表和安装预览从宿主/商店返回的验签 manifest 展示身份、版本、入口、依赖、能力用途和更新说明。仅 Lib 展示真实业务 OpenAPI；App 展示精确后端依赖。

开发可设置 `PISHOO_WORKSPACE_BACKEND` 后运行 `bun run dev`；`/api` 和原生 `/std` API 代理到该地址。`bun run typecheck` 检查前端及浏览器测试。Playwright 用 `PISHOO_WORKSPACE_E2E_BACKEND` 指向隔离后端；`e2e/package-spec.spec.ts` 使用完整 mock 校验无 OpenAPI 的 App 展示、静态入口和自动安装预览，不访问实际身份数据。

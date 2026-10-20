# Workspace 普通用户前端

此目录是独立 App，挂载 `/std/workspace/`，使用相对静态资源和 hash 导航。包内没有 atom、OpenAPI 或逐文件静态操作；宿主从实际 public 文件树派生 GET/HEAD 路由，所有文件仍经过 daccess。

`wasmer.toml` 的 `package.pishoo.dependencies` 精确声明同一发布者的 `profile@0.1.0` 和 `message@0.1.1`。Profile 后端负责资料与头像，原生 Contact 负责联系人与申请投递，Message 后端负责聊天；原生联系人、ACL、配置和制品管理仍调用 `/std` API。应用商店安装 App 会自动补装缺失后端，复用相同版本并拒绝冲突；本地文件安装要求后端已经安装。

Workspace 不随宿主发行包交付或首次启动自动安装。配置身份、启动 Pishoo 并安装提供 `genmeta package install` 的新版 gmutils 后，由身份所属账户运行 `genmeta package install std.dhttp.net/workspace --id YOUR_IDENTITY`。该命令调用在线安装 API，要求 App 和精确依赖已发布到商店。

宿主通过 Git 依赖使用 daccess 的 `feat/contact-acl-v2` 分支，具体提交由根目录 `Cargo.lock` 固定，无需同级仓库。

不编译宿主即可运行 `python3 pishoo/tests/check-contact-migration.py` 检查 SQLite 迁移。脚本读取 Cargo 已缓存的锁定版 daccess；缓存缺失时先运行 `cargo fetch --locked`，也可设置 `DACCESS_PATH=/path/to/daccess` 指定待检查源码。

从当前源码本地启动完整 Workspace，在仓库根目录运行：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk python3 std/apps/workspace/dev.py --id YOUR_IDENTITY
```

需要 Python 3、Rust（含 wasm32-wasip2 target）、Bun、wasm-tools、WASI SDK 和支持 `package build/install/list/remove` 的新版 genmeta。`--id` 使用本机已经配置、已启用的身份；缺少 config.db 时先用 `genmeta identity ensite --id YOUR_IDENTITY` 启用。先停止已有 Pishoo 服务，脚本会增量构建并启动当前分支宿主，等待管理入口就绪，再构建并签名三个包。

**每次运行都重建这三个包的开发数据。** 构建签名成功后，脚本按 Workspace → Profile → Message 的顺序卸载旧包，停止宿主，删除目标身份 `db/<publisher>/{message,profile}` 后端业务目录，并清除 `/std/message`、`/std/profile`、`/std/workspace` 及其子路径的 ACL 和审批记录，然后重启宿主，依次安装 Message、Profile、Workspace。聊天记录、个人资料和头像会重置。Workspace 为静态 App，没有宿主业务目录。身份凭据、联系人记录、原生配置及其他应用数据保留；其他 App/Lib 若依赖这两个后端，会在卸载前报错。宿主沿用当前账户的身份扫描和数据库迁移逻辑。

Rust 版本和 WASI target 由仓库根目录的 `rust-toolchain.toml` 声明；Lib 构建脚本在各自项目目录运行 Cargo，沿用该工具链。

Lib 构建会清除继承的全局 `RUSTFLAGS`、`CARGO_ENCODED_RUSTFLAGS`，避免宿主链接参数传入 WASM 链接器。WASM 专用 Rust 参数可通过 `CARGO_TARGET_WASM32_WASIP2_RUSTFLAGS` 设置。

默认使用目标身份签名；通过 `--publisher NAME` 可选择另一个已启用且有本机凭据的发布者。已有同名包时应沿用其发布者；发布者不一致时在卸载前报错。用 `--genmeta /path/to/genmeta` 可指定新版工具。每次运行的制品和宿主日志保存在 Cargo target 目录下的 `workspace-dev/run-*`。

成功后在 AnySee 等支持 DHTTP 的浏览器中，以目标身份访问 `https://YOUR_IDENTITY/std/workspace/`。终端保持运行，Ctrl+C 只停止本次启动的宿主；再次运行脚本会重新清理开发数据。脚本失败也会清理本次宿主进程，安装仍遵循逐包提交语义，已成功安装的依赖保留。

构建使用支持现有 bun.lock 的 Bun（本次验证为 1.4.3）：

```sh
sh build.sh --publisher YOUR_NAME.dhttp.net
```

生成三份独立签名 WEBC，需要本机发布者凭据及 WASI SDK：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh package.sh --publisher YOUR_NAME.dhttp.net
```

输出为 `target/workspace-dist/{message-lib,profile-lib,workspace-app}.webc`，不上传商店或修改用户数据。设置 `GENMETA` 可以选择新版 genmeta 打包工具。构建输入生成在 `package-dist/`，不进入 public 文件树；源文件不会被发布者参数改写。

应用列表和安装预览从宿主/商店返回的验签 manifest 展示身份、版本、入口、依赖、能力用途和更新说明。仅 Lib 展示真实业务 OpenAPI；App 展示精确后端依赖。

开发可设置 `PISHOO_WORKSPACE_BACKEND` 后运行 `bun run dev`；业务和原生 `/std` API 代理到该地址。`bun run typecheck` 检查前端及浏览器测试。Playwright 用 `PISHOO_WORKSPACE_E2E_BACKEND` 指向隔离后端；`e2e/package-spec.spec.ts` 使用完整 mock 校验无 OpenAPI 的 App 展示、静态入口和自动安装预览，不访问实际身份数据。

公开介绍页使用 `/std/workspace/#/intro`，独立展示头像、名称和添加好友入口，不加载 Workspace 管理页面。我的信息页可预览并复制分享链接；访客在新标签页打开自己的 Workspace，目标身份完整预填。匿名访客先填写自己的身份名称，不允许添加自己。介绍页及静态资源、`GET/HEAD /std/profile/context` 和 `GET /std/profile/avatar` 都须由主人通过普通 daccess ACL 显式放行；安装不自动授权。

`e2e/introduction.spec.ts` 保留 public-introduction 分支的浏览器验收，并适配现行 std 路径、原生能力目录和 Message。

联系人页统一使用卡片，展示 pending 等关系状态及无聊天权限的联系人；常规目录隐藏 blocked。移除需处理、已保存、已拉黑分类与 saved 标记。按别称、对方昵称、身份名选择显示名称并排序，同名按完整身份名稳定排序，排序后分页。聊天入口与 @ 联系人候选沿用该顺序。

管理接口统一为 `/std/contact` 与 `/std/contact/list`，头像仅 GET `/std/contact/{name}/profile/avatar`，使用对方提供的图像，不允许本地覆盖。具体契约与迁移规则见独立 daccess 仓库的 `SPEC.md`。

添加联系人只提交关系申请，不再先查询对方能力或勾选申请权限；对方资料离线时使用身份名继续提交。本地先保存投递任务，首次收到认证回执后创建绑定对方 SubjectId 的 pending 联系人。接受联系人只修改关系状态。聊天入口对未拉黑联系人开放，发送后由对方 ACL 决定；异步待审核显示“等待审核”，后台重试实际消息。

审批页合并联系人关系待办和实际接口访问待办，访问待办区分实时连接与异步记录。批准／拒绝会保存该用户对相应接口的 ACL allow／deny，后续持续生效，可在权限规则中修改；不再使用一次性允许或决定有效期。客户端默认携带 If-Reviewed: break，400 need_review 与普通业务错误分开处理。实时待办等待期间只在内存，请求 Future 取消或超时后移除。

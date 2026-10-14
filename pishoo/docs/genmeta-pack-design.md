# genmeta pack 实现设计与 Agent 任务书

日期：2026-10-14。状态：待实现。本文件与 [upload](genmeta-upload-design.md)、[install](genmeta-install-design.md) 一起描述用户确认的三个工具；不表示代码或商店已经上线，也不修改冻结接口清单。

## 1. 目标和已确认边界

`genmeta pack` 将本地构建产物打包成一个已经签名、验证通过的 WEBC v3 App 或 Lib，供 `genmeta upload` 发布。App/Lib 类型、身份、版本及依赖来自 `wasmer.toml`，遵循 [App/Lib 软件包规范](package-spec.md)。

- 只接收本地目录或本地清单文件。
- 不获取 GitHub 仓库、远程 TOML、URL 文件或远程构建产物。
- 不运行项目的 Cargo、Bun、npm 或任意构建脚本；开发者先完成源码构建。
- 不联网解析运行依赖，不上传，不安装，不改变 ACL。
- `[package].repository` 只用于展示源码仓库地址，保留在签名 manifest 中。
- App 和 Lib 独立打包，不产生 bundle、App 运行时模型或安装账本。

实施前阅读本仓 `AGENTS.md`、`design/README.md`、`design/pishoo-interfaces.md`；gmutils 的仓库约束也适用。禁止修改 h3x/dhttp/dquic/qtls 接口。

## 2. 命令契约

```sh
genmeta pack [PATH] [-o FILE.webc] [--id PUBLISHER]

genmeta pack
genmeta pack ./workspace-app
genmeta pack ./std/libs/profile/wasmer.toml -o ./dist/profile-lib.webc
```

| 参数 | 行为 |
| --- | --- |
| `PATH` | 默认 `.`。目录读取其中 `wasmer.toml`；文件直接作为 TOML 清单读取 |
| `-o, --output` | 相对当前工作目录解析；默认 `<清单目录>/dist/<name>-<version>-<kind>.webc` |
| `-i, --id` | 可选身份核对。规范化后必须等于清单 publisher；省略时由清单 publisher 选择本机凭据 |
| `-h, --help`、`-V, --version` | 与 genmeta 现有命令保持一致 |

不提供 kind/name/version/dependencies 的命令行覆盖，不提供 unsigned、force、自动上传或远程 source 选项。身份固定来自当前账户的 `~/.dhttp`，不读取 DHTTP_HOME，不增加 home 参数。

## 3. 项目输入

最小 App 目录：

```text
workspace-app/
├── wasmer.toml
└── public/
    ├── index.html
    └── assets/
        ├── main.js
        └── main.css
```

```toml
[package]
name = "dev.dhttp.net/workspace"
version = "0.2.0"
description = "用户工作台前端"
license = "Apache-2.0"
repository = "https://github.com/genmeta/workspace"

[package.pishoo]
kind = "app"
title = "Workspace"
entry = "/"
changelog = "首次发布独立前端。"
dependencies = [
  { publisher = "dev.dhttp.net", name = "workspace", version = "0.2.0" },
  { publisher = "dev.dhttp.net", name = "chat", version = "0.2.0" },
]

[fs]
"/public" = "public"
```

最小 Lib 目录包含 `wasmer.toml`、`lib.wasm`、`metadata/openapi.json`。Lib 清单使用 `kind="lib"`、显式 `[package.pishoo.host]`，禁止 entry，使用唯一 `[[module]] name="backend", source="lib.wasm"` 和 `[fs] "/metadata"="metadata"`。无依赖显式 `dependencies=[]`；无宿主能力显式 host 空表。具体字段不在本文件重新定义。

所有 module/fs 本地路径相对清单所在目录解析。映射可以指向已经构建的 `dist`，只收集实际映射树，不自动收集源码、node_modules 或整个项目。

## 4. 执行和发布文件

1. 解析输入、清单及输出位置，立即拒绝错误参数。拒绝将 URL 当本地资源。
2. 校验包身份、版本、类型和声明格式；确认实际静态文件或 backend/OpenAPI 存在。
3. 在创建暂存文件前确认最终输出及暂存位置均在所有内容映射目录之外；使用实际解析后的目录关系，不能只比较字符串前缀。例如映射项目根 `.` 时，默认项目内 dist 输出应报错并提示使用外部 `-o`，不能暗中排除文件。
4. 选择与 publisher 一致的真实本机凭据，用已有 LocalAuthority 签名。凭据缺失/不匹配时拒绝，不修改清单以迁就凭据。
5. 复用已有 WEBC v3 Writer 和包级签名域构造签名包；签名覆盖最终 manifest、atoms、内容卷，重构后验证最终内容摘要与签名一致。
6. 使用现有 pishoo 整包验证路径校验最终包。Lib 还检查组件 imports/exports 和编译兼容性，不实例化 guest。
7. 在输出所在文件系统暂存并同步，然后使用不覆盖已有目标的原子发布操作；目录同步沿用平台现有支持。不能先检查存在再普通 rename 覆盖，必须覆盖并发创建场景。
8. 只有最终文件已提交才输出成功。失败删除本次暂存文件，保留旧输出和原输入。

遵循包规范现有全部限额、路径/符号链接规则，不增加另一套限额。已有同名输出一律失败，不提供 force；用户可以显式选择另一个输出或自行移除旧文件。

## 5. repository 展示数据

`[package].repository` 是可选非空 HTTPS URL；拒绝非字符串、无主机、用户名密码或非 HTTPS 协议。允许 GitHub 等源码仓库站点，不通过网络验证链接，不下载、不执行链接内容。省略时不生成空字符串或 null 占位字段。

保持原字符串进入 `package.wapm.repository`，并由既有 `package_metadata` 方法体派生可选 `repository` JSON 字段，使 `pishoo app/lib check`、安装返回和已安装/已加载目录可复用。不在 `package.pishoo` 再存副本，不从 OpenAPI、商店未签名外链或 Git URL猜测值。

前端显示由 install agent 负责；商店元数据透传由 upload agent 负责。host `package_metadata` 和规范中 repository 的语义由 pack agent 唯一维护。

## 6. 输出和错误

stdout 在成功时输出一份 JSON：`id, kind, version, file, size, package_sha256`；`file` 为绝对路径。stderr 用于校验、签名等进度与错误，不输出凭据。对子进程 check 的 JSON 先捕获解析，不与最终 JSON 混排。

使用退出码 0 表示成功；非零表示失败。沿用 launcher 的错误机制；帮助/版本正常退出，参数错误沿用 clap。需要区分“缺少凭据”“缺少构建产物”“输出已存在”“输出位于映射目录内”和“签名/组件验证失败”，不要求新增公共 Error 变体。

## 7. 实现位置与并行文件归属

推荐把现有 `pishoo/tools/package-webc.rs` 提升为随 Pishoo 安装的正式二进制 `pishoo-pack`，保留既有 example 调用兼容。它是独立命令入口，不新增 Pishoo 库公开函数。`genmeta pack` 使用普通 `pack.rs`，通过参数数组启动该工具，不拼接 shell 命令。优先查找与 genmeta 同目录的工具，再按 PATH 查找；缺失时明确提示需要安装包含此工具的 Pishoo。是否存在工具不能用失败后在线下载来补救。

pack agent 唯一负责以下共享文件：gmutils 的 `genmeta/src/main.rs`、`genmeta/Cargo.toml`、根 `Cargo.toml`、`Cargo.lock` 及共享 genmeta 命令文档；Pishoo 的 `pishoo/Cargo.toml`、打包工具、发行打包文件、package_metadata 和 package-spec。修改 xtask 的方法体/脚本即可完成时，不新增其冻结字段。

在 gmutils launcher 中预留并最终接入三个具体变体，字段均为当前命令所需：

```rust
Pack { path: PathBuf, output: Option<PathBuf>, id: Option<String> },
Upload { file: PathBuf, id: Option<String> },
Install { target: String, id: Option<String>, version: Option<String>, kind: String },
```

`path` 的 clap 默认值为 `.`；install.kind 默认 `app`，值限制为 app/lib，无需自定义枚举。三个模块使用普通 mod 声明；不引入新的 State/Context、Guard 或包身份结构。协作接缝为：

```rust
// gmutils/genmeta/src/pack.rs
pub(super) async fn run(path: PathBuf, output: Option<PathBuf>, id: Option<String>)
    -> Result<(), snafu::Whatever>;
// gmutils/genmeta/src/upload.rs，由 upload agent 实现
pub(super) async fn run(file: PathBuf, id: Option<String>)
    -> Result<(), snafu::Whatever>;
// gmutils/genmeta/src/install.rs，由 install agent 实现
pub(super) async fn run(target: String, id: Option<String>, version: Option<String>, kind: String)
    -> Result<(), snafu::Whatever>;
```

这些是本任务的具体 CLI 集成提案，不把它们扩展为 Pishoo 库 API。复用 main 已有 Whatever 错误转换。upload/install agent 将所需直接依赖和自身验证命令交给 pack agent，不能各自编辑共享 Cargo/launcher 文件。pack agent 负责最后接入三个模块、更新依赖锁和运行集成检查；在模块未齐全时，不提交空实现或宣称 launcher 已完成。

## 8. 验收与交付

- 目录、显式 TOML、默认参数、非 UTF-8 文件路径以及带空格目录能正确处理；URL 输入失败。
- 使用临时测试身份打包最小 App、真实兼容 Lib；现有宿主 check 验签通过，篡改内容失败。
- manifest 中 repository 保留；缺省可用；错误协议/类型被拒绝。
- App 含 backend、Lib 缺 metadata、版本不一致、非法文件/超限、publisher 不匹配均失败且不留下成功输出。
- 输出映射自包含、目标已存在及并发创建目标均失败并保留原文件；验证失败不能覆盖输出。
- 安装发行包后，`genmeta pack` 不依赖源码 checkout 或 `cargo run --example`；验证发行脚本确实携带工具。
- 不访问真实用户私钥、上传线上包或部署商店；测试用隔离身份。适当运行 Rust check/test、格式检查及发行契约检查。

交付包含实现、命令帮助/用户示例、测试结果、发行工具接入证据、给其他两个 agent 的共享入口完成状态。任何超出上述具体变体/接缝/可选展示字段的冻结成员或 SQL 修改，先列出成员级差异并取得用户授权。

# genmeta pack 实施与交接

日期：2026-10-14。已实现源码与本地验收；尚未构建或发布新的 DEB/RPM/Homebrew 发行包。

## 使用

```sh
genmeta pack
genmeta pack ./workspace-app
genmeta pack ./std/libs/profile/wasmer.toml -o ./dist/profile-lib.webc
genmeta pack ./workspace-app --id dev.dhttp.net
```

需要安装新版 Pishoo 的两个正式二进制 `pishoo`、`pishoo-pack`。launcher 优先查找同目录的打包器，再通过 PATH 查找。独立工具接受同样参数；旧 `cargo run --example package-webc -- WASMER.toml OUTPUT.webc --publisher NAME` 参数继续兼容。新增二进制后，Cargo 的默认运行目标仍是 `pishoo`。

输入只限本地目录或 TOML 文件，所有构建产物路径相对清单目录；项目先自行构建。无 kind/version/dependencies 覆盖、unsigned、force、联网依赖解析、上传、安装或 ACL 写入。签名只加载 manifest publisher 对应的本机凭据；测试使用临时生成的身份，不访问真实用户私钥。

默认输出 `<清单目录>/dist/<name>-<version>-<kind>.webc`；显式输出相对当前工作目录。映射目录与输出/暂存位置按实际解析后的路径比较，拒绝自包含及符号链接别名。既有目标或并发新建目标均不覆盖。先构建包并签名，复核重建后的签名内容摘要，再捕获宿主 check 的 JSON；只有验证通过后才以 `persist_noclobber` 提交，并按 Unix 的现有支持同步父目录。

成功 stdout 仅含 `id, kind, version, file, size, package_sha256` JSON；错误和上下文写 stderr。路径通过 OsString/PathBuf 和独立参数传递，不拼 shell。JSON 的 file 为绝对实际路径；无法编码为 UTF-8 的路径仅在 JSON 展示时使用替换字符，实际文件操作保持原始路径。

`package.repository` 原字符串进入 `package.wapm.repository`，作为签名内容；必须是有主机、无用户名密码的非空 HTTPS URL。宿主既有 `package_metadata` 方法体派生可选展示字段，缺省不生成 null/空字符串，也不在 pishoo 注解存副本。package-spec 已同步语义。

## 验证结果

Cargo 在 `/private/tmp` 执行，以避开工作区父目录的开发 path patches；正式依赖锁不改变无关依赖版本。以下检查已通过：

- Pishoo `cargo check --locked --offline --bins --example package-webc`。
- Pishoo `cargo test --locked --offline --test pack_commands`：17 项通过。涵盖真实 App/兼容 Lib、宿主验签、签名篡改、repository、缺失/错误产物、版本不一致、非法路径/文件大小、符号链接、映射自包含、旧目标保留、并发创建目标、FIFO 不阻塞、帮助/版本及旧 example 参数。
- 设置 `GENMETA_PACK_TEST_BIN` 为已构建的 genmeta 后，同一组 17 项再次通过；临时复制 genmeta/pishoo-pack/pishoo 三个二进制、清空 PATH，在临时项目中完成真实 genmeta pack。
- gmutils `--test pack_launcher`：3 项通过，验证同目录优先、PATH 回退、独立参数、stdout JSON、缺少或失败工具。
- gmutils launcher 单元集成：32 项通过，1 项 upload 网络集成按其测试声明忽略；无失败。首次发现的 upload 容器解析和 install 身份测试失败已由并行工作修复，最终结果通过。
- Pishoo xtask `--test release_contract`：7 项通过。
- 本任务 Rust 文件的 rustfmt 检查、两仓 git diff --check、发行 shell 脚本 bash -n 通过。

macOS APFS 不允许创建非法 UTF-8 文件名；该平台的测试验证 CLI 能接收原始 OS 参数并返回本地路径错误。支持这种文件名的平台会执行真实非 UTF-8 项目/输出文件打包分支。本机未执行 Linux 发行包安装或该分支的 Linux 运行。

发行接入证据：Brew 脚本同时拷贝 pishoo/pishoo-pack，formula 同时安装两个程序；DEB rules 使用非 sparse 拷贝两个程序；RPM 增加 Source1、安装项及 files 项，并拷贝真实打包工具到 SOURCES。契约测试检查这些接缝。

## upload / install 共享入口状态

gmutils 当前 `main.rs` 已用普通 mod 连接 pack/upload/install 三个实际模块，并使用设计指定的具体参数变体与 Whatever 错误转换；Cargo 与锁文件含并行模块的直接依赖。无空实现。pack 本身不新增依赖，复用已有 Tokio 和 Snafu。

pack 独占负责的 Pishoo Cargo、打包器、repository package_metadata 方法体、package-spec 和发行脚本已完成。本任务没有修改 h3x/dhttp/dquic/qtls 接口、Pishoo 有状态成员、SQL 或前端展示代码。其他任务同时编辑的 upload/install 模块、商店、前端及原有未跟踪设计文件均保留。

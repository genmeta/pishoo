> 2026-10-15：统一命令与构建流程见 [genmeta package](genmeta-package.md)。下文的打包细节由 pishoo-pack 承担；genmeta package build 先执行本地构建配置。

# pishoo-pack 实施与交接

日期：2026-10-14。已实现源码与本地验收；尚未构建或发布新的 DEB/RPM/Homebrew 发行包。

## 使用

```sh
pishoo-pack
pishoo-pack ./workspace-app
pishoo-pack ./std/libs/profile/wasmer.toml -o ./dist/profile-lib.webc
pishoo-pack ./workspace-app --id dev.dhttp.net
```

需要安装新版 Pishoo 的两个正式二进制 `pishoo`、`pishoo-pack`。launcher 优先查找同目录的打包器，再通过 PATH 查找。独立工具接受同样参数；旧 `cargo run --example package-webc -- WASMER.toml OUTPUT.webc --publisher NAME` 参数继续兼容。新增二进制后，Cargo 的默认运行目标仍是 `pishoo`。

输入只限本地目录或 TOML 文件，所有构建产物路径相对清单目录；项目先自行构建。无 kind/version/dependencies 覆盖、unsigned、force、联网依赖解析、上传、安装或 ACL 写入。签名只加载 manifest publisher 对应的本机证书和私钥，使用 qtls 的现成签名能力；不加载传输 Endpoint，不要求 OCSP 缓存。最终仍由宿主完整验签，证书与私钥不匹配时不发布输出。打包回归使用临时生成的身份。

默认输出 `<清单目录>/dist/<name>-<version>-<kind>.webc`；显式输出相对当前工作目录。映射目录与输出/暂存位置按实际解析后的路径比较，拒绝自包含及符号链接别名。既有目标或并发新建目标均不覆盖。先构建包并签名，复核重建后的签名内容摘要，再捕获宿主 check 的 JSON；只有验证通过后才以 `persist_noclobber` 提交，并按 Unix 的现有支持同步父目录。

成功 stdout 仅含 `id, kind, version, file, size, package_sha256` JSON；错误和上下文写 stderr。路径通过 OsString/PathBuf 和独立参数传递，不拼 shell。JSON 的 file 为绝对实际路径；无法编码为 UTF-8 的路径仅在 JSON 展示时使用替换字符，实际文件操作保持原始路径。

`package.repository` 原字符串进入 `package.wapm.repository`，作为签名内容；必须是有主机、无用户名密码的非空 HTTPS URL。宿主既有 `package_metadata` 方法体派生可选展示字段，缺省不生成 null/空字符串，也不在 pishoo 注解存副本。package-spec 已同步语义。

## 验证结果

Cargo 在 `/private/tmp` 执行，以避开工作区父目录的开发 path patches；正式依赖锁不改变无关依赖版本。以下检查已通过：

- Pishoo `cargo check --locked --offline --bins --example package-webc`。
- Pishoo `cargo test --locked --offline --test pack_commands`：17 项通过。涵盖真实 App/兼容 Lib、宿主验签、签名篡改、repository、缺失/错误产物、版本不一致、非法路径/文件大小、符号链接、映射自包含、旧目标保留、并发创建目标、FIFO 不阻塞、帮助/版本及旧 example 参数。
- 设置 `GENMETA_PACK_TEST_BIN` 为已构建的 genmeta 后，同一组 17 项再次通过；临时复制 genmeta/pishoo-pack/pishoo 三个二进制、清空 PATH，在临时项目中完成真实 pishoo-pack。
- gmutils `--test pack_launcher`：3 项通过，验证同目录优先、PATH 回退、独立参数、stdout JSON、缺少或失败工具。
- gmutils launcher 单元集成：32 项通过，1 项 upload 网络集成按其测试声明忽略；无失败。首次发现的 upload 容器解析和 install 身份测试失败已由并行工作修复，最终结果通过。
- Pishoo xtask `--test release_contract`：7 项通过。
- 本任务 Rust 文件的 rustfmt 检查、两仓 git diff --check、发行 shell 脚本 bash -n 通过。

macOS APFS 不允许创建非法 UTF-8 文件名；该平台的测试验证 CLI 能接收原始 OS 参数并返回本地路径错误。支持这种文件名的平台会执行真实非 UTF-8 项目/输出文件打包分支。本机未执行 Linux 发行包安装或该分支的 Linux 运行。

发行接入证据：Brew 脚本同时拷贝 pishoo/pishoo-pack，formula 同时安装两个程序；DEB rules 使用非 sparse 拷贝两个程序；RPM 增加 Source1、安装项及 files 项，并拷贝真实打包工具到 SOURCES。契约测试检查这些接缝。

## upload / install 共享入口状态

gmutils 当前 `main.rs` 已用普通 mod 连接 pack/upload/install 三个实际模块，并使用设计指定的具体参数变体与 Whatever 错误转换；Cargo 与锁文件含并行模块的直接依赖。无空实现。pack 本身不新增依赖，复用已有 Tokio 和 Snafu。

pack 独占负责的 Pishoo Cargo、打包器、repository package_metadata 方法体、package-spec 和发行脚本已完成。本任务没有修改 h3x/dhttp/dquic/qtls 接口、Pishoo 有状态成员、SQL 或前端展示代码。其他任务同时编辑的 upload/install 模块、商店、前端及原有未跟踪设计文件均保留。

## std 安装、好友与消息流程测试

`pishoo/tests/run-std-workflow.py` 枚举 `std/libs` 与 `std/apps` 的真实源码，构建后调用 Pishoo 的 `pishoo-pack`，然后运行独立的 HTTP/3 集成测试。标准发布者签名使用本轮用户提供的仓库根 `std.dhttp.net.crt.pem` / `std.dhttp.net.key.pem`；只在临时 HOME 中复制凭据并设置私钥权限，不修改用户身份目录。

```sh
WASI_SDK_PATH=/path/to/wasi-sdk python3 pishoo/tests/run-std-workflow.py \
  --genmeta /path/to/new/gmutils/genmeta --bun /path/to/current/bun
```

开发依赖图可通过 `--cargo-config /path/to/local-patches.toml` 指定。`--skip-build` 复用已构建的宿主和 std 产物；`--packages` 仅用于缩小诊断范围，完整验收不设置它。Bun 须能读取当前冻结锁文件；本轮使用 1.4.3。

测试生成两个独立的测试证书身份 Alice/Bob，设置临时 CA 为该测试进程的信任根，保留真实 QUIC/mTLS 校验。开始时确认双方没有任何已加载 App/Lib；全部安装通过主人身份的 `PUT /std/sys/package/{libs|apps}/<name>` 完成，并核对 config.db 登记和展开目录。随后通过主人 API 明确开放具名访客的能力目录及 Profile GET，再从各自出站队列发起申请。目录开放与消息授权均是显式步骤，安装不授予权限。

双方同时发起申请后，覆盖六种顺序：先接受双方再授权、先授权双方再接受、两侧各先授权再接受、Bob 先接受授权后 Alice 接受授权、Bob/Alice 先授权再依次接受、Alice 先接受授权后 Bob 接受授权。每一步调用现有联系人工作函数与 WASI CLI 导出，使用公开 refresh API 调度查询，不写入联系人/ACL/Message 数据库伪造状态。每一步实际投递探测消息，核对未同时具备 active 联系人和当前本地消息 Allow 时返回403；每种顺序最终核对出站请求 active 和两方向消息 sent/received。

另测联系人仍 pending 时提前批准请求仍不可收信、重复审批请求去重、审批后重试、同一 client_message_id 幂等收信，以及审批不产生持久 Allow。批准由下一次请求消费，再次投递重新返回202；再次批准后，同一消息返回200和原消息 ID，不重复入库。请求审批按当前 daccess 的真实身份与请求元数据作用域查询；不同审批场景用不同 query 区分。最后重新加载包并核对联系人和消息行数保持，Message 新库不创建私有审批表。测试直接使用生产 Server/Router 和工作函数；运行入口的周期调度由测试逐步驱动。

2026-10-14 本轮完整源码构建与真实网络验收通过：Workspace / Calendar App，Profile / Message / Calendar Lib，当前均为0.1.0，全部使用实际 std.dhttp.net 证书签名。两个隔离身份各自从零 App/Lib 开始，共十次 PUT 安装成功；六种顺序各完成两方向消息投递，所有中间权限探测、审批与重载断言通过。最终独立网络测试为1项通过、0失败，耗时159.36秒；同时打包回归18项、Workspace 审批回归17项、独立 launcher 拷贝运行检查及普通 guest 出站拒绝检查通过。本轮未执行公网 DNS/NAT 或浏览器交互验收。

流程中修复四个实际问题，均只修改既有方法体或注册现有标准接口，无新增冻结成员、数据库字段或私有授权容器：

- 离线 pack 不再依赖传输 OCSP；仍核对证书、签名与完整包内容，私钥不匹配时不发布。
- 宿主补充标准 wasi:random/insecure-seed，复用现有 WasiCtx 的随机资源，使 Calendar 的 Rust HashMap 构建产物可以正常校验和运行。
- Workspace 授权期间被交叉申请同步激活的联系人，重复接受返回409后重新读取并核对原联系人 ID、subject_id 和 active 状态；拉黑或替换的联系人不作为成功处理。
- Message 的 can_receive 从现有原生 ACL 读取当前规则，避免联系人授权快照为空时错误显示不能收信。接收请求仍由真实 daccess 授权及当前联系人状态核对。

本机完整输出保存在 `target/std-workflow-results/2026-10-14.log`；打包与审批回归输出分别为同目录的 `pack.log`、`approvals.log`。测试身份及其私钥、数据库由临时目录自动清理，现有 ~/.dhttp 身份未清空或重装。

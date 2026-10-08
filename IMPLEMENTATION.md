# 第一版实施记录

日期：2026-09-26。这是实现和验收记录，接口以 `design/README.md` 为准。

## 2026-10-08：删除宿主命令 exec

- 按用户要求删除 exec 模块、`/exec` 路由、Server.exec_tasks、ServerConfig.exec、专用错误 BackendUnavailable/Cancelled/Closed、专用测试及 test.sh；移除直接 nix 依赖和 Tokio process feature，base64 仅保留为既有证书测试的开发依赖。同步冻结接口、架构和配置 API 文档，删除 exec 接口清单，不新增替代状态或接口。
- 新建 config.db 继续使用 schema v1，settings 只含 listen；已有库的旧 exec 列原样保留但不读取或更新。配置 API 仅返回 listen，提交 exec 字段按未知字段返回400；`/exec` 不再保留为内置命名空间。现有测试改为验证 listen 的保存/重启生效，并核验新库列及旧列兼容。
- 验证：108项默认库测试通过，3项默认跳过；其中两项本机 TCP 代理测试先受沙箱端口限制，再于允许绑定本机端口的环境下补跑通过。另行显式运行本机 QUIC 配置 API 用例通过，确认 listen/代理保存、授权及重启生效。`cargo check --locked --offline -p pishoo --all-targets`、修改 Rust 文件格式和 diff 检查通过；未修改实际身份数据库、线上 DNS 或重启现有服务。

## 2026-10-08：运行入口只保留启动、DNS 维护和退出

- 用户明确选择删除每日 OCSP 自动更新和 SIGHUP 重载，更新统一重启。移除 `Server::reload`、renew_ocsp/reload_profiles、信号与定时更新分支、运行中身份增删、关闭身份筛选和重载恢复逻辑；删除对应重载测试及 systemd ExecReload，同步冻结接口和使用说明。
- 保留启动身份校验与 OCSP 缓存准备、共享 Network、串行 Lib 加载、DNS 续期/失败重试/地址变化、Ctrl-C/SIGTERM 退出和应用/mDNS 收尾。发布 future 不再装箱，select 直接按 next_due 等待，不保存额外 timer；run 当前83行，不新增结构、字段或替代 API。
- 验证：117项库测试通过、3项默认跳过；配置 API 本机 QUIC 用例另行显式通过，确认写入不更新当前 exec/代理配置，关闭后新建 Server 才读取新值。all-targets 编译、修改 Rust 文件格式及 diff 检查通过。库测试首次受沙箱端口权限限制，获准本机端口后复测通过；未重启日常进程或修改线上 DNS。
- 完整 xtask release_contract 离线运行缺少 arc-swap 1.9.2，未改依赖或锁文件；将受影响的纯标准库安装契约测试原样提取，用 rustc 单独执行通过，临时测试文件与程序已清理。

## 2026-10-08：DNS 记录自然过期

- 用户取消主动撤回：删除 `dns::withdraw` 及身份删除、退出时的调用；外网地址为空时不发送空发布、不安排续期，已有 DDNS 记录按租期自然过期。同步冻结清单和 DNS 详细设计。
- 删除身份仍清除 mDNS 本机应答，已有远端缓存按 TTL 过期；退出仍关闭应用与自有 mDNS 资源。移除仅为撤回准备的启动失败分支，保留统一收尾及发布批次边界，不增加结构、字段或替代接口。
- 验证：4 项 DNS 单元测试通过，覆盖空地址停止续期和非空发布失败重试；`cargo check --locked --offline -p pishoo --all-targets`、修改 DNS 文件格式检查及 diff 检查通过。实际组播测试未重跑，未修改线上 DNS 或重启现有进程。

## 2026-10-04：好友申请的反向聊天授权确认

- 接收方复用现有 Workspace 联系人 worker，对已激活、涉及聊天能力且存在同名同 SubjectId 入站申请的联系人调用 daccess 既有 `GET /contact/self`。批准时唤醒，启动及每30秒核验；无需反向重复申请，不新增结构、成员、跨模块接口或数据库表。
- 确认使用实际连接的对端 SubjectId，并验证响应描述的是当前本端名称与 SubjectId；只采纳 active 状态的实际 granted_access，不把申请中的 offers 当作确认。写入前重新核对联系人记录编号、身份和激活状态。已撤回/拒绝/不存在的权限记录为未授权，临时网络或服务错误保留最近观察并重试。
- 验证：Workspace 24项测试通过，覆盖待审批隔离、offer不自动确认、尚未建立反向记录后的重试、撤回、临时失败、错误身份和无效响应。真实本机 QUIC 用例通过：Alice/Bob各申请一次，接收方无需另发申请即可确认反向授权、出现在正常目录并双向投递消息。构建和diff检查通过。
- 更新本次会话启动的实例后，现有 code.alice.smith 对 alice.smith 的目录响应已由 `remote_chat_granted: null` 自动变为 `true`，`chat_available: true`；未手工修改授权规则或直接修补数据库。
- 新建 Alice→Home 的实测暴露首次确认时序：被动方先查询得到404，主动方随后才取回批准结果并创建反向记录。首次404现在保持未确认，已有观察后404仍撤销原确认；联系人列表、详情及聊天页在前台等待反向确认时每3秒刷新状态，离页清理计时器。前端类型检查与构建通过，按用户要求不执行浏览器验证。

## 2026-10-04：身份凭据验证失败跳过

- 按用户要求，启动和 SIGHUP 新身份加载仅跳过 InvalidIdentity，日志包含规范化身份名与原因；后续身份继续加载，下一次 SIGHUP 重试。Endpoint 的身份文件/证书/私钥错误归入现有 InvalidIdentity；OCSP 获取超时也按身份凭据失败处理，证书有效期失败在发起 OCSP 请求前返回。配置、数据库、Lib 及全局网络错误仍返回，不新增结构、字段、错误变体或跨模块接口。
- 已加载身份在 SIGHUP 读取证书失败时跳过本次重载并保留原有内存凭据和应用资源；证书链改变仍要求重启。未启动日常身份进程或修改其数据库/凭据。
- 验证：隔离 OpenSSL 凭据回归显式通过，覆盖缺失/损坏/过期证书、坏私钥跳过、后续有效身份加载、修复身份再加载，以及配置/Lib 错误仍返回；普通重载回归覆盖损坏/缺失凭据保留资源与修复后重试。Pishoo 库测试118项通过、3项默认跳过；上述隔离测试另行执行通过。Rustfmt与git diff检查通过。验证使用现有临时 Bun 1.4.2；旧 PATH Bun 无法读取锁文件，未改依赖或锁文件。完整库测试需允许本机 HTTP 测试端口。

## 2026-10-04：正常启动初始化与安装接入

- 用户确认使用 home 下的身份，应用目录直接放在身份根级，不再使用 server.conf。正常 Server.load 在成功加载身份凭据后创建缺失的 db/file/lib/logs/repo/templates；Workspace 创建现有 assets/profile。默认 home 继续由 dhttp-home 的 User scope 解析，DHTTP_HOME 可覆盖。没有新增类型、字段、方法签名、跨模块接口、初始化状态或账本；辅助算法限定在所属模块内。
- load_server_config 对缺失或完全空白、未声明版本的数据库事务创建 schema v1：settings 恰好一行 listen=3（内外网均监听）/exec=0，代理为空。已有库校验后读取。配置 API 的 GET 使用只读路径，已有 Server 的 reload 也不重建缺失/空库；SIGHUP 新发现身份仍走正常加载。未知版本、部分建表、损坏及路径符号链接拒绝，不覆盖既有数据。
- 新 access 数据库通过当前 daccess API 在临时库建表、写入 POST /contact 的 Allow/Named（**）规则，再用 SQLite backup 生成独立完整快照、校验/同步后发布。原生 daccess v1只加载、不补已删除默认规则；原生v0先在副本验证，再保留 access-v0-backup-*.db（包含已提交WAL内容），由库自身事务在原文件上升级。旧0.8.2 location_rule_sets/location_rules、未知结构和不支持版本明确报错并保留原库，无旧格式自动重置。所有者权限仍由库派生；匿名申请拒绝，Chat独立审批。
- Workspace/Chat 在各自既有 open/migrate 中检查完整性，空库事务创建现行schema8/4，已有库在任何DDL前校验版本及必要列，不自动修补缺失业务表。不导入示例关系或消息；重启保留资料、会话和投递数据。新Unix目录0700、数据库0600，已有权限不改。
- 安装脚本继续不操作用户home。DEB/RPM/Homebrew仅打包现有主程序，移除已删除worker/SSH二进制的拷贝和旧实例配置安装；systemd删除不支持的-t/-s/PIDFile，用SIGHUP重载、默认SIGTERM退出，不以5秒超时打断应用退出。服务运行用户和DHTTP_HOME由部署者明确设置，Homebrew提示按当前用户启动。修正打包测试遗留的beta.2版本断言，保持现行稳定0.8.2版本不变；xtask锁文件仅同步未使用的本地patch元数据，未升级依赖版本。
- 验证：新增9项初始化回归和2项配置读取/重载边界测试通过，覆盖默认权限、重复启动保留删除规则、空库与未发布临时数据恢复、未知/旧格式逐字节保持、原生v0升级含WAL备份、资料会话保留及符号链接拒绝。工作区完整回归通过：114项库测试与3项DNS测试；之后补充的2项边界测试单独通过。真实Workspace/Chat QUIC用例扩展为接收者无config.db启动，检查默认设置和具名申请能到达库处理器，再验证管理员拒绝规则及完整投递流程。all-targets检查、修改Rust文件格式检查、shell语法及diff检查通过。xtask的6项发布契约与6项安装hook测试通过。未构建跨平台发行包、安装系统服务、重启现有进程或修改日常home数据。

## 2026-10-03：H3 配置 API 第一版

- 用户批准按资源拆为 `GET/PATCH /sys/settings` 和 `GET/PUT /sys/proxies`，启动与重载均挂载。仅新增已批准的 `setup::config_router(profile, endpoint)` 跨模块函数；复用现有配置结构、身份 Endpoint、daccess middleware 和 schema v1，不增加字段、数据库表或传输接口。
- settings PATCH 严格校验 listen 整数 0..3、exec 布尔值及未知字段；proxies PUT 校验完整数组、重复 location、保留路径和本机 HTTP/TCP 上游。共用磁盘配置的校验逻辑，保留未写路径与显式 `/` 的区别；SQLite 即时事务避免部分写入和并发 PATCH 丢失字段，数据库失败整体回滚。
- 请求在 daccess 授权后复核已验证 Visitor 与 Endpoint 的名称及 owner_hash；响应支持 v1 的 Accept-Versions 协商并禁止缓存。`/sys` 纳入既有保留路径检查。接口未限制本地网络来源；当前握手摘要不提供该信息。
- 写成功仅表示数据库保存，代理通过 SIGHUP 重载，listen/exec 需重启。原生 `pishoo-client` 增加 put/patch JSON 命令，使用现有 H3 RequestWriter。参数和调用示例见 [配置 API](pishoo/docs/config-api.md)。
- `cargo check --locked --offline -p pishoo --all-targets` 通过；允许本机 socket 后 `cargo test --locked --offline --workspace` 通过（105项 Pishoo 库测试、3项 DNS 测试，真实网络与组播用例默认跳过）。修改文件格式检查和 `git diff --check` 通过。
- `DHTTP_TEST_OPENSSL=/opt/homebrew/bin/openssl cargo test --locked --offline -p pishoo --lib config_api_real_h3_persistence_authorization_and_reload -- --include-ignored --test-threads=1` 显式通过：临时证书及本机真实 QUIC/H3 验证配置读写、持久化、生效边界、重载后路由保留、非法规则不落库、其他及匿名身份拒绝、daccess 对具名 owner 的拒绝规则。该用例需要 socket 权限并独占全进程测试 TLS/DNS/home，不修改日常身份或线上 DNS。

## 2026-10-03：本机双 Workspace 与 AnySee 客户端 OCSP 兼容

- 用户要求启动两个 Workspace，随后明确要求取消客户端必须携带 OCSP 的限制。qtls ClientVerifier 仍执行证书链、有效期及握手签名验证，未带客户端 OCSP 时接受经验证的证书；带了 OCSP 时仍验证签名、时效、撤销状态和证书绑定。服务端 OCSP 要求不变；没有新增配置、结构、成员或 HTTP/3 帧格式。
- qtls 全部23项测试通过，新增用例覆盖无 staple 的认证与 RemoteAuthority、错误/不匹配 staple 拒绝、无 staple 时证书签名和有效期仍检查。Pishoo/原生客户端重新构建通过；Workspace/Chat 的真实 QUIC 业务及发送前身份校验用例复测通过。
- 在 `target/workspace-dual` 启动 alice.smith 与 code.alice.smith 两个身份，使用现有证书和私钥引用、官方预取并验证的 OCSP，配置 listen=1/exec=0，独立创建 config/access/workspace/chat 数据库。未修改日常身份、默认身份、原服务或线上 DNS。
- 测试 daccess 只允许这两个具名身份互相 POST /contact，Chat 仍需用户在界面审批。后台进程信息保存在 `target/workspace-dual/pishoo.pid`，输出保存在 pishoo.log；进程持续运行供用户测试。
- 本机旧版 genmeta curl 原先收到 TLS alert113，改动后两个 Workspace 首页、同名身份 context 与联系人申请规则查询均成功，HTML/JS/CSS 可通过实际 mDNS/QUIC 读取。浏览器本身由用户手动验证，不通过 computer use 操作。
- `target/workspace-dual/open-workspaces.command` 可由用户手动运行，启动两个独立 AnySee user-data-dir；各自使用 `target/workspace-dual-clients/alice` 与 `code` 的 DHTTP_HOME/default identity，避免两个普通标签页共享全局默认身份。

## 2026-10-03：接通 Workspace/Chat Endpoint 出站

- Server.load 为 Workspace 与 Chat 的既有 OutboundTransport 装配当前具名 Endpoint；两个 trait 均直接由 dhttp::Endpoint 实现。没有新增 Pishoo 生产结构、字段、连接池、身份缓存或后台协调器。重载复用既有 Workspace/Chat，Lib 的 WASI HTTP 出站仍拒绝。
- Workspace 使用当前 Endpoint 的内存 LocalAuthority 派生 sender_subject_id，借助 Empty/WndBuf 接缝中的 WndBuf/RequestWriter 发送有限请求、显式 shutdown，并从响应扩展中的已验证 RemoteAuthority 提取 remote_subject_id。保留 JSON 请求、响应头、15秒总期限与1MiB响应上限；远端资料已有的2秒期限仍优先生效。
- 用户明确批准 dhttp::Request 增加 expected_remote_owner_hash 与 expect_remote_owner_hash，以及无载荷 Error::RemoteIdentityChanged。校验使用 Network 返回的实际 H3 连接，发生在 open_bi 与任何 HTTP 头/Body 发送之前；复用的连接也逐请求核对 owner_hash。body/write 保留期望身份。Chat 将已保存 SubjectId 解析为 OwnerHash，身份不匹配交给既有 worker 阻止投递并清除远端授权观察。
- 新增显式真实网络验收 `pishoo/tests/unit/server/workspace_network.rs`，由临时 OpenSSL CA、SHA-256叶证书、DHTTP SKI、匹配私钥和签名 OCSP 驱动真实 UDP/TLS/H3；不关闭 TLS 身份、名称或 OCSP 验证。三个临时 Server 的管理请求也通过具名 QUIC，使用临时 profile/数据库，不读取或改写日常身份数据。测试独占进程级 TLS/DNS/DHTTP_HOME，故默认 ignored，须单独运行。
- 真实验收通过：Alice/Bob 两个独立身份读取 Receiver 资料，投递联系人申请，接收者激活并授权 Chat，发送者通过 /contact/self 轮询观察授权，然后由各自生产 Chat worker 完成消息投递并在接收者数据库读取。daccess 未允许 /contact 时仍返回403；验收显式配置临时接收者的申请权限。错误 owner_hash 的 Empty/WndBuf 请求不会到达应用，过期联系人 pin 的真实 Chat 作业变为 blocked 并清除授权观察。

验证（Bun 1.4.2；网络测试使用 `/opt/homebrew/bin/openssl`）：

- `cargo check --locked --offline -p pishoo --all-targets` 通过。
- `cargo test --locked --offline --workspace` 通过：99项 Pishoo 库测试与3项 DNS 测试；新真实网络用例默认跳过，另行显式运行通过。既有 mDNS 组播用例仍默认跳过。
- `DHTTP_TEST_OPENSSL=/opt/homebrew/bin/openssl cargo test --locked --offline -p pishoo --lib workspace_chat_real_quic_delivery_and_pre_send_identity_check -- --include-ignored` 通过。
- 相邻 dhttp 的 `DHTTP_TEST_OPENSSL=/opt/homebrew/bin/openssl cargo test --locked --offline -p dhttp --lib --tests -- --include-ignored` 全部50项通过，包含真实 QUIC、身份验证与连接复用回归。
- 修改 Rust 文件格式检查与两仓 `git diff --check` 通过。冻结成员变更已记录于 [dhttp 清单](design/dhttp-interfaces.md)；业务接缝见 [Workspace/Chat 清单](design/workspace-chat-interfaces.md)。

边界：本轮验证本机真实 QUIC 和完整业务链路；跨设备公网、NAT 与应用级两进程 smoke 尚未验收。联系人是否允许申请、是否授权 Chat 仍由当前 daccess 及能力决定控制。

## 2026-10-03：定位并修复 QUIC 测试的 Crypto(51)

- 临时记录 qtls 的原始错误后确认，客户端收到的是 `InvalidCertificate(UnsupportedSignatureAlgorithmContext)`，证书签名算法 OID 为 `1.2.840.10045.4.1`（ECDSA/SHA-1），当前 provider 不支持；对外仅保留 TLS alert 51。此前的证书和私钥也是实际生成的材料，错误来自生成器依赖 OpenSSL 的默认签名算法，不是必须换成商业 CA 证书，也没有据此判定 QUIC 协议实现有错。
- `dhttp/tests/support/credentials.rs` 明确以 named_curve P-256、SHA-256 生成自建 CA、叶证书、匹配私钥和真实 OCSP 签名。启动 QUIC 前验证私钥有效性、证书与私钥导出的公钥一致、CA 链和 hostname，以及 OCSP 响应签名。测试可用 `DHTTP_TEST_OPENSSL` 指定实际执行文件，避免子进程命中不兼容的 openssl 实现；此次指定本机 `/opt/homebrew/bin/openssl`。
- 移除全部临时 TLS 诊断日志，没有修改 qtls、h3x 的生产接口或关闭证书、域名、OCSP 验证。临时身份目录仍由现有测试清理。
- `DHTTP_TEST_OPENSSL=/opt/homebrew/bin/openssl cargo test --locked --offline -p dhttp --lib --tests -- --include-ignored` 全部50项通过：40项库测试、6项 bootstrap 配置测试和4项集成测试，包含真实 UDP/TLS/H3、具名与匿名请求、已验证对端身份、连接复用、反向请求及证书名称不匹配拒绝。两仓 diff 检查和修改测试文件的格式检查通过。
- 下节记载的 Crypto(51) 阻塞已解决。这次验收覆盖 dhttp 底层；Pishoo 应用级两进程 QUIC smoke 与 Workspace/Chat 生产出站仍待后续。

## 2026-10-03：适配现行 Empty/WndBuf 出站接口

- 底层同期将 Endpoint 恢复为具名身份，`name()` 返回 `&str`。用户曾批准标准 Body await，随后在底层任务明确删除该分支；本轮以最终仅支持 Empty/WndBuf 的接口为准，不恢复 Request<Body>、send_body_request 或响应 Body 上传控制。
- 用户批准正向代理本轮先做字节流转发，暂不支持请求 trailers。`forward_dhttp` 保留同名与 owner_hash 校验、URI/Host 重写和可信 extensions 清理；以现有 WndBuf/RequestWriter 流式转发 DATA，EOF 时 shutdown。声明 Trailer 头的请求返回400；未声明 trailers 或 Body 读取错误使上传失败，丢弃未完成 writer 以取消上游，不静默丢弃 trailers。响应已交付时不能追溯改变响应状态；响应 Body/trailers 保持原生传递。
- 客户端改用 RequestWriter 写入和显式 shutdown，有限请求使用初始 Bytes 窗口，流式 Echo 使用同一个 writer 持续上传。smoke 的 trailers 项现在只验证响应 trailers。交互示例仅支持当前 QUIC 入口，README 不再指向已删除的 TCP mock 构建。
- 单元测试通过内存证书构造现有 Endpoint，不再依赖日常 DHTTP_HOME 或修改进程环境。qbase 仅作为生成测试身份的 dev-dependency；没有新增生产有状态类型、字段或跨模块接口，h3x 未修改。
- 使用临时 Bun 1.4.2：`cargo check --locked --offline -p pishoo --all-targets` 通过，`cargo test --locked --offline --workspace` 全部94项库测试通过（含本机回环代理）；新增测试覆盖有界写入的背压/EOF、请求 trailers 与 Body 错误拒绝。修改文件的 Rust 格式检查和两仓 `git diff --check` 通过，Python 交互入口的参数检查通过。
- dhttp 当前38项库测试通过。真实 `quic_roundtrip --include-ignored` 复测先遇到临时 OpenSSL EC 私钥编码不被 provider 接受；测试 fixture 显式指定 named_curve 后已能加载凭据，但仍在首个 WndBuf 请求的 TLS 握手返回 `Crypto(51)`，尚未进入 Pishoo 应用链路。本轮不宣称真实 QUIC 端到端通过，Workspace/Chat 的生产出站仍待接入。

## 2026-10-03：清理正式清单的 TCP mock 依赖

- 删除 Pishoo 的 `tcp-mock = ["dhttp/tcp-mock"]` feature，以及 `setup-tcp-demo` example 对该 feature 的门槛。保留旧测试素材，后续 QUIC smoke 适配另行处理；这不恢复 TCP mock 传输。
- 使用正式清单离线同步 `Cargo.lock`，仅删除 dhttp 的 `async-stream` 与 h3x 的 `scopeguard` 依赖边，未升级依赖版本。没有修改冻结接口或生产 Rust 代码。
- `cargo metadata --locked --offline --no-deps --format-version 1` 与 `git diff --check` 通过。
- 本机 Bun 1.2.16 无法读取现有前端锁文件；验证改用临时目录的 Bun 1.4.2，以 `--frozen-lockfile` 安装前端依赖，保持系统 Bun 和仓库前端锁文件不变。
- 使用上述临时 Bun 执行 `cargo check --locked --offline -p pishoo --all-targets`：依赖解析与 Workspace 前端构建通过，随后 Pishoo 库报告10处编译错误，库测试报告18处编译错误。剩余错误涉及 `Endpoint::name()` 的 `Option<&str>` 返回值、DHTTP 正向代理的旧 Body 请求 await 接缝，以及测试身份构造的参数变化，归下一步底层 API 适配。本阶段不宣称完整构建或真实 QUIC 端到端通过。

## 2026-10-02：rebase daccess 与 Workspace/Chat

- 当前适配分支 `feat/rebase-daccess` 以 `origin/feat/daccess@9b733c5` 为基线，重放14个本地提交。重放前的完整工作保存在 `feat/pre-daccess-rebase-20261002@33f2d50`；原 main 保持原提交。range-diff 确认其余13个补丁相同，首个重构提交仅调整与远端旧架构文件的冲突。
- daccess 固定 Git revision `cf8f72f4e6bedbd7c98648ffee31053cb509b395`。审批改为202、持久记录和按 Visitor 查询；联系人改为申请队列及 application_id 轮询，ContactNotifier 回调接缝删除。
- 完整 Workspace 前端保留在 `pishoo/workspace`，应用页面源码、样式和锁文件与目标分支一致。`src/workspace/mod.rs`、`src/chat/mod.rs` 改名为普通 `workspace.rs`、`chat.rs`，旧占位 HTML 由 dist 资源替代。
- Server 直接拥有 Workspace/Chat，重载复用资源，关闭时停止并等待现有 worker。保留本地 Sandbox、Lib、exec、配置和本机反代实现；上述生产文件与 rebase 前快照一致。
- 修正目标分支已有的测试导入、身份 fixture、过期时间和计时精度问题；前端 E2E 补齐当前目录 API 的 mock，并按当前审批详情抽屉和 Allow 动作校准预期。

验证：

- Rust 集成编译通过。完整库测试92项首次90项通过，随后按目标库补齐审批 Visitor 及缺失身份的状态码预期，6项授权/联系人测试复测全部通过；其余86项无需变更。
- 前端 `bun run build` 与 `bun run typecheck` 通过，验证使用临时 Bun 1.4.2。
- 桌面 mock E2E：21项通过，1项移动端专用用例按配置跳过。浏览器使用独立临时配置下的本机 Chrome；不使用日常浏览器 profile。
- `git diff --check` 与 Rust 格式检查通过。

限制：Rust 检查使用临时清单，仅将 `tcp-mock = ["dhttp/tcp-mock"]` 改为空 feature，实际生产源码与其他依赖不变。正式清单仍保留这一已知底层 feature 冲突，等待用户要求的底层接口稳定后统一处理。当前 Workspace/Chat 的生产 OutboundTransport 尚未配置；远端资料返回503，申请与消息保留在数据库队列等待接入。原网络测试保存于 `pishoo/tests/deferred/workspace_network.rs`。本次未修改相邻 daccess、dhttp、h3x 的源码或日常 profile 数据库，不宣称真实跨端出站已验收。

当前接口以 [Pishoo 清单](design/pishoo-interfaces.md) 和 [Workspace/Chat 清单](design/workspace-chat-interfaces.md) 为准。以下保留此前各轮的实施记录。

## 修改前的 Git 基线

| 仓库 | 分支 | 基线提交 |
| --- | --- | --- |
| Pishoo | main | `4b36f36` |
| dhttp | main | `a82d654` |
| h3x | feat/wasm | `fa5835c` |

实现已在基线之后单独保存阶段提交，后续文件整理与功能实现分开审阅。h3x 保持结构、字段和接口，只修复读取消息头期间取消 future 的原生流清理。

文件整理前的实现检查点：Pishoo `58fafc0`、dhttp `502ad17`、h3x `592989b`。

Sandbox 拆分前的 Pishoo 文件整理检查点：`a6a19be`。

WASM 职责集中到 Sandbox 前的 Pishoo 检查点：`bc30231`，保存上一轮 Sandbox 拆分。

## 文件组织

测试代码统一放在各 crate 的 `tests/` 下：`unit/` 存放需要访问私有实现的单元测试，`support/` 存放内存流等测试工具，`cases/` 存放较长集成测试的分组。`src/` 仅保留测试模块挂载声明，不为测试扩大生产 API 的可见性。

Pishoo 使用普通 `mod` 声明和同名 `.rs` 文件，子模块放在同名目录。库入口分别为 `pishoo/src/pishoo.rs` 和 `gateway/src/gateway.rs`，由 Cargo 的 `[lib].path` 指定。Sandbox 合并为四个文件：`sandbox.rs` 负责组件管理和 API 路由，`sandbox/runtime.rs` 负责编译、Store、执行和响应体，`sandbox/host.rs` 负责 WASI HTTP 出站拒绝接缝和身份宿主能力，`sandbox/manifest.rs` 负责清单校验。测试继续放在 `tests/unit/`，通过 `#[path] mod` 挂载；测试文件也使用同名 `.rs`，不使用 `mod.rs`。Workspace 前端现位于 `pishoo/workspace/`，构建后内嵌 dist 资源。此次接入保留 Workspace/Chat 分支原有的业务测试布局。

Pishoo 的其他实现按同样原则合并：`server.rs` 集中运行循环和单身份服务；`routes.rs` 集中分发与静态文件，`routes/access.rs` 负责授权和管理入口，`routes/proxy.rs` 负责反代；配置归 `setup.rs`，单命令宿主执行归 `exec.rs`。

h3x 同样合并同类型实现：帧载荷收拢为 `frame/payload.rs`，SETTINGS 构造并入 connection，QPACK 编码状态与算法并入 encoder，请求/响应读取并入 read。dhttp 的 Endpoint 收拢为入口/连接/服务、网络维护、消息与 Body 适配三个文件；identity、home、access 中同职责的实现片段合并。两仓测试片段合并回所属测试模块；库根与模块根采用同名 `.rs`。

## 当前实现

- h3x：既有消息读取方法在等待 HEADERS 前后保留明确的取消所有权；裸流 Drop、正常 EOF 和半关闭约定保持原样。
- dhttp：全局 Network 持有以本端、远端规范化名称为键的 h3x 连接池；Endpoint 只持名称，不提供 close 或 stop_listening。同名 load 复用连接。Network 没有 shutdown，进程退出时结束其剩余传输与维护任务。
- dhttp 操作等待：删除 `OPERATION_TIMEOUT` 及开流、消息头和 Body 读写的单次超时；保留连接超时与流背压。出站请求 future 或响应 Body 提前丢弃时，现成 scopeguard 中止尚未结束的上传任务。
- Pishoo：schema v1 数据库读取、启动时扫描身份及 SIGHUP 显式重载、Server 直接持有 Router 与 Sandbox、Sandbox 直接持有 Lib、串行重载、加载失败直接返回、删除时撤销入口。监听任务不保留句柄。
- 路由：受目录能力约束的流式静态文件、精确/最长前缀本机 HTTP/TCP 代理、同名身份专用的固定前缀 DHTTP 正向代理、WASM 显式方法路由、daccess 授权与202持久审批、管理 API、Workspace 联系人申请队列与轮询，以及完整 Workspace/Chat 界面。Workspace/Chat 生产出站已接到现有 Endpoint，并通过本机真实 QUIC 业务验收。Lib 的 WASI HTTP 出站暂不实现。
- WASM：每身份一个 Sandbox，持有 Lib 集合、共享 WasmRuntime 引用和 WASM 任务跟踪器，集中组件与执行管理；单 Lib 的 `/data` 权限、实际 Store 内存/fuel 限制、无总时长上限的受跟踪 guest 任务、流式响应、身份签名与出站拒绝 hook。
- exec：每 Server 的 `settings.exec`、与 Lib 合并的 `POST /exec` Router 分支、daccess 加同名身份准入、直接 argv、输入输出和单次执行限制、受跟踪的 Child 取消和回收。程序使用 Pishoo 当前非 root 服务账号权限，没有文件或网络隔离。

原交互终端的安装布局探测、WASM shell WIT、帧协议和平台 helper 方案已按用户的新 v1 决定移除。单命令 exec 的标准输入输出采用有界 JSON/base64；不使用 PTY、shell 解释或子进程 IPC。

用户明确确认的契约变更均已记录在冻结清单中：

- `ProxyLocation.proxy_pass` 改为 `http::uri::Parts`，保留未写路径和显式 `/` 的差异。
- Sandbox 集中持有 `libs: HashMap<String, Arc<Lib>>`、`runtime: Arc<WasmRuntime>`、`tasks: TaskTracker`；Server 删除独立的 Lib 集合和 WasmRuntime 引用，直接持有 `sandbox: Sandbox`。`run` 直接创建并共享 WasmRuntime。
- 移除 DaemonConfig、Daemon、TerminalPolicy 和实例配置文件；`run` 的局部变量负责身份扫描、监听、串行重载与关闭，每个 Server 直接读取自己的 `db/config.db` 并持有 exec 任务跟踪器。
- Sandbox 构造接收 WasmRuntime，`load_libs(&mut self)` 扫描成功后直接更新集合，`api_router` 从当前集合构建路由；同步 `close(&mut self)` 关闭任务登记并清空 Lib；`wait(&self) -> Result<()>` 保持不变。Server 显式合并管理、Lib API、exec 和 `/file/{*path}` 静态路由，设置代理 fallback 与统一授权层；WASM 内部类型和 manifest 验证统一归 sandbox，`validate_lib` 的根级公开导出不变。

Sandbox 的关闭分为停止准入、清除 Lib 与等待任务回收；在途 Lib 执行继续运行直到自行结束；关闭等待仍有15秒上限。Server 继续直接持有 Endpoint、授权、完整 Router 与 exec 任务跟踪器，Lib 和 exec 都不限制并发数，删除 `Sandbox.lib_slots`、`Invocation.permit`、`StoreData.permit`、`Invocation::new` 的 permit 参数及 `Error::Capacity`；保留单次执行的内存和 fuel 限制，不设 WASM 总执行期限。不新增内部锁、取消信号、派生计数或通用策略容器，也不把 Sandbox 描述为操作系统进程或容器隔离。

启动和重载都先让 Sandbox 扫描并加载 Lib，再构造 Lib Router 与完整 Router；扫描或编译失败直接返回，保留旧集合和路由。成功后同步替换完整 Router 与配置。Lib API 不自动写 daccess 规则；未匹配时使用 daccess 的默认策略，已有管理员规则保持原样。

## 本地配置

`DHTTP_HOME` 必须在进程启动前设置。程序不修改进程环境；没有实例配置文件或实例数据库。每个 Server 只读取自己的 `db/config.db`。

每个身份保持 `ssl/`，静态文件放在 `file/` 并通过 `/file/{*path}` 访问，组件放在 `lib/<id>/lib.wasm`。组件顶层须有唯一的 `pishoo:openapi` 自定义段。

每个身份的 `db/config.db` 使用现有 schema v1：

```sql
PRAGMA user_version = 1;
CREATE TABLE settings (listen INTEGER NOT NULL, exec INTEGER NOT NULL);
INSERT INTO settings VALUES (1, 0);
CREATE TABLE proxy_locations (location TEXT NOT NULL, proxy_pass TEXT NOT NULL);
```

listen 的 0/1/2/3 分别代表关闭/内网/外网/两者；exec 只能为 0/1，作为本 Server 单命令 exec 的开关，只开放给同名已验证远端身份。改变 listen 或 exec 需要重启。代理支持 `127.0.0.1:8080` 和 `http://127.0.0.1:8080[/路径]` 等回环 HTTP/TCP 上游。未写路径的上游保留原路径，显式写 `/` 的上游按匹配前缀替换路径。

## 验证

取消 WASM 总执行期限后：Pishoo 49 项库测试通过。模拟时间推进 31 秒的未轮询流式响应仍在执行，随后取消可回收；无限循环 guest 可被取消，未取消时因 fuel 耗尽而退出。`cargo fmt -p pishoo --check` 与 `git diff --check` 通过。

2026-09-27 TCP mock 端到端验收：先前的 HTTP/1 curl 原型已由跨进程 h3x/TCP 后端替换。dhttp 的泛型 Network 在测试构建选择 TcpTransport，以单条回环 TCP 连接承载双向请求流和单向控制/QPACK 流；默认构建仍选择 QuicTransport。Pishoo 的 `Server.listen` 仍只调用 Endpoint.listen。`./pishoo/tests/h3x-tcp-smoke.sh` 分别启动 Pishoo 服务端进程和原生 dhttp Endpoint 客户端进程；客户端确认静态文件、前缀/精确代理、三个实际 WASM Lib、八个并发流、256 KiB Echo、POST Echo、请求 trailers、多值响应 trailers、双向流式 Echo 及提前丢弃响应后继续请求均成功，HTTP 响应版本为 HTTP/3，两个 Echo 数据块分别在下一块输入和上传 EOF 前到达。此验证不覆盖 QUIC、TLS 对端认证、QUIC RESET 语义或路径发现。

Lib API 默认拒绝登记后：Pishoo 50 项库测试通过，新增测试确认首次重载写入 `Deny/All`、管理员改为匿名允许后再次重载不会覆盖。h3x/TCP 集成脚本重新通过，演示环境显式放行的三个 Lib API、静态文件与两种代理均成功。

2026-09-28 简化 Lib 加载与重载后：移除上述自动登记；`load_libs` 直接更新 Sandbox，`api_router` 从当前集合建路由。Pishoo 52 项库测试通过，覆盖加载失败保留旧版本、删除 Lib 或整个 lib 根目录后的入口撤销、重载不写 ACL、未匹配规则的非 owner 拒绝与 owner 允许。

TCP 样例不再写根路径的匿名 Allow，只精确放行测试使用的 GET 静态/代理路径和从 WASM 清单解析出的 Lib 方法与路径；新增跨进程请求确认 `GET` 和带请求体的 `POST /unlisted` 均返回 403。TCP mock 的 StopSending 只释放本端读取方向，避免把双向流的响应方向误当作 request reset；它仍不模拟完整 QUIC STOP_SENDING 错误码传播。

交互示例 `pishoo/examples/echo-interactive.py` 在 TCP 模式自动启动独立服务端与 `pishoo-client`，单个双向流 POST 接收多行输入并逐行回显；QUIC 模式连接已有身份与地址。Echo 组件按 4 KiB 块读取并逐块 flush 响应。

`tcp-mock` 构建的 `pishoo` 主入口与默认构建相同，从现有 `DHTTP_HOME` 加载身份、配置及 Lib。固定演示身份、WASM 和匿名规则改由独立 `setup-tcp-demo` example 创建；演示放行路径与方法从写入后的 `lib.wasm` OpenAPI 清单解析，不在服务入口硬编码。

2026-09-28 本机代理调整后：`proxy_pass` 接受裸回环地址端口或 `http://` 回环 URI，`proxy` 不再接收 dhttp Endpoint；以 Hyper HTTP/1.1 连接本机上游，按目标设置 Host、清理逐跳头，不自动生成 `X-Forwarded-*`。Lib 出站保持 dhttp。57 项 Pishoo 库测试通过，实际本机上游测试覆盖 POST、路径和 Host 改写、响应、多值头，以及上传未结束时逐块返回响应和模拟空闲31秒后的继续传输；h3x/TCP 跨进程 smoke 改由独立本机 HTTP 服务提供代理上游，也验证了双向流式响应在上传 EOF 前到达。

示例目录只保留交互入口和 WASM 编写示例；h3x/TCP 回归脚本与其 Rust 客户端、样例环境初始化移到 `pishoo/tests/`，打包与重建工具移到 `pishoo/tools/`。交互入口复用该客户端，编译期选择默认 QUIC 或 `tcp-mock`，对单个 POST 长连接持续上传和回显。TCP 模式逐行验收确认首行在发送第二行前返回，第二行在上传 EOF 前返回；QUIC 模式已完成编译链接，真实连接依赖外部身份与路径环境，未在本地样例中验收。

首批实现验证结果：Pishoo 47 项、dhttp 27 项、h3x 149 项通过。

Sandbox 拆分后：Pishoo 50 项测试通过，覆盖不同身份的执行槽隔离、重载沿用同一 Sandbox、等待实际任务/permit 回收，以及关闭超时仍保留未完成任务的资源所有权。

WASM 职责集中到 Sandbox 后：Pishoo 56 项测试通过，编译与格式检查通过。新增验证覆盖候选摘要改变时保留旧版本与取消状态、Router 快照共享执行额度、HEAD/OPTIONS 显式声明、API 命名空间隔离，以及通过实际 WASM 执行验证 Sandbox 关闭回收。

取消 Lib 并发限制后：Pishoo 56 项测试通过，覆盖8个同时在途的实际 WASM 请求、关闭后旧 Router 拒绝执行，以及任务回收、内存/fuel 和超时约束。

改用普通 `mod`、库根文件改名及 Sandbox 四文件合并后：Pishoo 56 项测试、工作区全部目标编译检查和 `cargo fmt --all --check` 通过。

扩展到三仓的模块合并后：h3x 162 项测试通过；dhttp 工作区启用 access 的 migration/http 特性后，256 项测试及20项文档测试通过。两仓在临时副本使用 stable 完成全量测试，应用补丁后逐文件确认源码与测试副本相同；Pishoo 再使用原仓库 nightly 工具链运行56项测试，验证实际三仓依赖链。三仓 nightly 格式检查通过。

文件整理后保持上述测试通过，并复验 dhttp 子库：home 31 项、identity 112 项、access 启用 migration/http 时 49 项及 20 项文档测试、log 12 项单元测试及 25 项集成测试。

单命令 exec 替代交互终端、同步当前 Network 生命周期后：Pishoo 51 项单元测试通过。exec 测试覆盖正常输出与退出码、单次主动取消后的直接子进程回收、无限输出触发上限并结束进程、分片内存 Body 请求得到 JSON 结果、关闭任务登记后拒绝待续传 Body、超过四个并发命令，以及 `/exec` 在应用 Router 中挂载。可信远端身份的正向路径尚未在真实跨端请求中验证；主动脱离进程组的后代不在本版保证内。

2026-09-28 全 Pishoo 精简后：移除实例锁、listener JoinSet、整体停机期限及 `Sandbox::verify_libs`；身份或 Lib 加载失败直接返回。运行入口与单身份服务合入 `server.rs`。51 项库测试通过，h3x/TCP 跨进程 smoke 脚本通过；该脚本需允许本机回环端口绑定。

随后按用户明确决定移除 `Invocation.producer_cancel` 与 `LibResponseBody` 的两个 `cancel_on_drop` 字段。Body 丢弃不另行取消 guest；Lib 删除或关闭不再主动取消在途执行。没有后续 I/O 的 guest 可能继续运行，直到自行结束。

随后按用户要求删除 Server.close 的停止监听调用。Pishoo 不保留 listener 句柄，删除身份时关闭应用 Router 与任务，原监听持续到进程退出；同名身份恢复需要重启进程。当前 Pishoo 51 项库测试与 h3x/TCP smoke 脚本通过。

随后按用户要求删除 dhttp 的 `OPERATION_TIMEOUT` 及全部使用点。dhttp 库测试 17 项通过，新增测试覆盖请求 future 和响应 Body 提前丢弃时上传任务的取消；`cargo fmt -p dhttp --check` 与两仓 `git diff --check` 通过。

随后用户批准暂缓 Lib 出站：删除 `HostOutgoing`、`StoreData.outgoing` 和 `Invocation.endpoint`，移除出站子任务与取消链。`StoreData.deny_outgoing` 是 Wasmtime 所需的无状态 hook，始终拒绝 guest HTTP 出站；身份验证仅支持本端与当前握手对端。TaskTracker 直接跟踪 guest，反代本机 HTTP/TCP 保持独立。Pishoo 53 项库测试（含需要本机回环的代理测试）及所有 target 编译检查通过。

随后用户批准删除 `LibResponseBody`：收到 outparam 后直接适配 Wasmtime 原生响应 Body，成功响应不再保存 guest JoinHandle 或等待其结果；TaskTracker 继续跟踪 guest。无响应时仍等待任务以报告错误。删除两项只验证旧包装行为的测试，并将上传错误测试调整为验证原生 Body 结束和任务回收。Pishoo 51 项库测试、所有 target 编译检查、格式及 diff 检查通过；其中两项本机 TCP 代理测试在允许回环绑定的环境下通过。

随后用户批准不改 h3x、仅修正 dhttp 响应 Body 失败路径：转发 Body 返回错误时，先通过现有 `Response<Write>::cancel(H3_REQUEST_CANCELLED)` 取消响应流，再结束并发写入。内存 H3 测试现验证客户端收到部分数据后，继续读取会得到 `H3_REQUEST_CANCELLED`，而不只检查服务端错误。dhttp 的 17 项库测试及 8 项集成测试、格式和 diff 检查通过；未改 h3x 结构或接口。

2026-09-29 用户批准 `/.pishoo/dhttp/{target}` 固定前缀的 DHTTP 正向代理。Pishoo 复用当前 Server 的 Endpoint，要求握手来访者同名且 SKI owner_hash 相同，重写目标 URI 与 Host，清理逐跳头和入站可信 extensions，流式传递请求及响应；配置反代仍只使用回环 HTTP/TCP，Lib WASI HTTP 出站仍拒绝。针对性测试覆盖根路径、query、编码路径、证书序号目标、非法目标及匿名拒绝；Pishoo 55 项库测试通过，其中两项本机 TCP 代理测试在允许回环绑定的环境中运行。真实跨端成功路径仍受下述 qconn 路径发现缺口限制。

随后按用户要求将该入口合并为单条 `/.pishoo/dhttp/{*path}` 路由，支持目标根路径有无末尾斜杠及子路径。路由级测试验证三种形式都进入 DHTTP 处理器，URI 测试验证带末尾斜杠与 query 转发为目标根路径；Pishoo 56 项库测试在允许回环绑定的环境中通过。

按用户要求，统一 DHTTP 通配路由保持纯转发。联系人申请改由 daccess 的 `POST /contact/{name}` 管理路由发起：daccess 调用扩展后的 ContactNotifier trait，Pishoo 用本身份 Endpoint 发送申请并从出站响应的 RemoteAuthority 提取 Bob SubjectId，daccess 随后调用现有 `create_contact` 写入 Alice 本地 Pending 与精确回调规则。Bob 首次 `POST /contact` 仍需其 daccess 策略允许或审批，Pishoo 不绕过授权。Pishoo 库编译检查与 56 项库测试通过；其中 2 项本机 TCP 代理测试需允许回环绑定。daccess 的 68 项库测试使用相同源码和临时清单修正已删除的可选 dhttp-identity 路径后通过，dhttp 的 18 项库测试通过。tcp-mock 不提供已验证对端证书，真实跨端成功路径仍待 QUIC 路径发现完成后验收。

在各仓库执行：

```sh
# Pishoo：标准内存 Body、真实 WASM Store、SQLite/daccess 与重载测试
cargo test --locked --offline --workspace
cargo check --locked --offline --workspace --all-targets
cargo fmt --all --check

# dhttp：h3x 内存双向流与监听生命周期测试
cargo test --locked --offline -p dhttp --lib --tests

# h3x：保持既有结构/接口的协议行为验证
cargo test --locked --offline --lib --test request_response --test stream_lifecycle --test wnd_cancel --test body_chunks --test transport_io_failure --test message_traits
```

流测试不要求真实 dquic 联网，覆盖提前响应、背压、多值 trailers、Body 读写结果和 HEAD/204/304。Pishoo 测试不持有 QPACK 或 H3 writer。


## 2026-10-03 DNS 解析与发布

用户明确批准 DNS 详细设计的六项具体接口与跨仓修改，并同步冻结清单。本轮实现：

- Pishoo 新增普通 dns 模块，进程入口装配 System/H3/mDNS 三源并订阅实际地址簿；监听登记成功后按 listen 范围维护发布批。地址变化、续期、SIGHUP、删除和退出共用串行批次边界；失败重试5秒、单次发布/撤回期限3秒、非空租期至少30秒，按请求开始时间加租期三分之一续期。未完成监听登记的发布资源在失败收尾前释放，不向这些身份发送发布或撤回。
- Server 仅新增 publisher 实际协议资源；证书链变更在 SIGHUP 时拒绝并要求重启。撤回使用已加载内存凭据，关闭等待前释放 publisher；无 DNS Manager、地址/发布镜像表、并发名额或新传输关闭接口。
- dhttp 新增 local_authority 和 ListenFuture。await listen 完成登记后交付生命周期，未 poll 直接 Drop 仍清理登记；共享连接和 socket 不关闭。
- qprotocol 新增只读 inner_bindings，SystemResolver 跳过 DHTTP 名称；ddns 的 H3Resolver 强持原 Endpoint，返回真实租期并检查缺失、重复、数值及清空语义。mDNS 标准查询按网卡流式返回，查询名规范化，同网卡/IP 的 QUIC 端口在发布时合并。
- DDNS 服务端保持原路径、请求编码、签名、鉴权与按完整 SKI 撤回语义，仅发布响应增加 DHTTP-DNS-Lease-Millis 和 no-store，查询成功响应增加 no-store。ddns 遵守 no-store/no-cache，保留普通 DNS TTL/max-age/Age 行为。

已完成验证：Pishoo 原96项库测试及新增3项 DNS 测试通过；全 workspace/all-targets 编译检查通过。ddns 87项库测试及7项接口测试、dhttp 40项库测试及8项集成测试、qprotocol 94项与qresolve 14项测试通过。另显式运行本机实际 QUIC/H3 的3项测试，验证 DNS origin 引导、监听与连接复用、真实租期、no-store 响应不进入包内 TTL 缓存、删除磁盘身份后继续发布及撤回；Pishoo 的实际 mDNS 测试通过，验证端口合并、同IP资源重建、名称/序号/Source、撤回和自有资源关闭。

DDNS 服务端30项 router 测试通过，包括租期头、查询 no-store 和按完整 SKI 撤回。该服务端仍按其原 Cargo.lock 的发布依赖验证，HTTP API 无迁移；父目录的开发版 Cargo patch 会混入当前本地 Rust 底层接口，因此此项使用相同源码与测试的独立验证清单，未修改服务端传输源码、生产清单或锁文件。按用户要求验证后删除临时副本与构建产物，后续不再创建临时构建目录。缓存清理后的最终编译复查在原仓库使用兼容 Bun 完成，未修改前端锁文件。

仍需上线服务端的租期响应头；未验收跨设备公网发布/访问、生产故障与续期的长期运行、真实 NAT 映射或打洞。mDNS 撤回是撤销本地应答，已有远端缓存按 TTL 过期；同一完整凭据多进程及超时请求晚写入的限制保持设计所述。


### 2026-10-03 本地真实身份与线上 DDNS 验收

按用户选择使用 `code.alice.smith.dhttp.net`。原证书与私钥只通过引用加载，原 `~/.dhttp` 配置、原数据库以及已运行的旧版 Pishoo 未改动。测试仅在现有 target 中保存小型配置与公开诊断，没有复制源码或创建临时构建目录。

- 本地凭据：从 `https://api.genmeta.net/ocsp` 取得 code 身份的当前有效 OCSP，状态 good。原目录缺少 ocsp.der，因此响应仅提供给隔离测试环境。
- 线上查询：匿名及具名 H3 查询 `code.alice.smith.dhttp.net` 均返回 `Crypto(113)` / TLS alert 113，尚未进入 lookup HTTP handler。
- 线上发布：使用实际 Pishoo QUIC 绑定 `192.168.5.179:51914` 和原身份内存签名调用 publish；同样在 TLS 握手失败，未取得发布成功响应或租期，不能宣称记录已写入。本机无公网/NAT发布成功验收。
- 阻塞证据：OpenSSL QUIC 探测默认 origin 及东京 `52.192.35.155`、欧洲 `63.186.89.109`、北美 `35.167.130.73`，均验证证书链和 ddns.genmeta.net hostname 成功、协商 h3，但均显示 `OCSP response: no OCSP response received`。这与客户端 code 证书的 OCSP 是两份独立的响应。
- 进一步确认：从线上握手取得 ddns.genmeta.net 服务器证书，为该证书调用官方 OCSP API，得到 good，`Response verify OK`。其 This Update 为2026-10-03 16:06:58、Next Update 为19:06:58（Asia/Shanghai）。获取接口可用；线上 DDNS 未在 TLS 握手附带自己的状态响应。当前 qtls 要求此响应，因此拒绝连接；未放宽证书/OCSP校验或使用其他传输绕过。
- 本地端到端成功：独立客户端进程通过 mDNS 解析和真实身份认证的 QUIC/H3 进入本次 Pishoo，再反代 `127.0.0.1:52155` 的 HTTP/1.1 上游。`/dns-live/probe.txt` 与 `/dns-live/%70robe.txt?probe=20261003`（URI authority 带证书序号0）均返回 HTTP/3.0 200，67字节正文逐字节符合上游；上游日志确认编码路径与 query 保留。该成功使用 mDNS，不能当作线上 DDNS 全链路通过。

本次补齐既有 CLI 的 System/H3/mDNS 解析源装配和 stdout flush，并给 ddns 既有发布/查询示例增加日志及租期输出；未新增有状态结构或修改生产接口。原仓库的 Pishoo 服务及客户端构建通过。测试 Pishoo 正常 SIGTERM 退出、测试 HTTP 上游已停止，测试身份引用与数据库已清理；公开握手与验收结果保存在 `target/dns-live`。线上 DDNS 需要在 TLS 握手发送并及时刷新自身的有效 OCSP，之后再复测发布、查询和线上租期。

## 尚未完成的验收

- 本轮已通过本机真实 QUIC/H3 与 DNS 引导测试；跨设备公网、生产联系人投递与 NAT 成功路径仍待验收。Workspace/Chat 的生产出站接缝以当前冻结清单为准，仍按既有决定暂缓。
- exec 使用服务账号权限，不提供 OS 沙箱。主动脱离本次进程组的后代不在本版回收保证内；不宣称支持交互终端或任意恶意命令的完整资源隔离。
- Workspace 当前提供列表查看和审批操作；完整联系人/规则编辑交互仍待完善。管理 API 已直接使用当前 daccess 库。

### 2026-10-03 线上 DNS、真实 NAT 与打洞端到端验收

按用户要求继续线上验收，使用隔离的 code.alice.smith 身份引用、临时数据库与本机 HTTP 上游；线上 DNS 临时替换与恢复已经用户明确批准。未改原身份文件或已运行的旧 Pishoo。

- 服务器缺 OCSP staple：客户端从官方接口取得当前 good 响应，qtls 仅对 ddns.genmeta.net 缺失 staple 的握手读取 DQUIC_DDNS_OCSP_FILE，沿用证书链/名称、证书绑定、签名、时效与撤销验证；未注入或注入损坏数据仍返回 alert113。没有新增结构或字段。
- 发布身份兼容：TLS 证书确实发送，但线上旧 qconnection 还依赖现有 ClientName 传输参数。具名 connect 从同一 LocalAuthority 填入该参数后，线上发布从401变为200，日志确认本端为 code.alice.smith.dhttp.net；匿名查询仍匿名。
- 用户要求先跳过未部署的租期头：仅缺头的HTTP200暂按300秒续期窗口（清空为0）成功，有头仍执行原校验；不宣称这是服务端确认的租期。发布、自动发布、退出撤回及恢复均成功。
- 线上 DNS 路径：临时发布实际内网QUIC绑定，独立客户端只注册System/H3，线上查询得到本次地址；两个带普通/编码路径与query的Pishoo反代请求均HTTP/3 200，67字节正文逐字节一致。
- NAT：先在新socket分类，后逐节点查询映射，避免前一次探测改变过滤条件。双方均为RestrictedPort，服务端192.168.5.179:65238映射到113.80.22.156:25303，客户端192.168.5.179:55748映射到113.80.22.156:25371。
- 打洞：通过线上DDNS发布44.253.170.203:20002中介记录，两端仅广告公网地址，撤去AddressBook中的LAN/loopback广告且不注册mDNS。具名QUIC/H3首条路径经中介，随后双方分别记录active/passive punch completed。8秒后的已验证路径含服务端实际socket直连113.80.22.156:25371，第二次HTTP/3反代仍200且正文一致。
- 范围：本次为同一真实RestrictedPort NAT后的两个独立进程，覆盖公网中介引导与NAT hairpin直接路径；尚未验证两个不同NAT、跨设备、Symmetric/Dynamic或长期映射保活。NAT装配在客户端example的serve/nat-get中执行，普通Network启动仍不自动启动探测，不把此验收算作生产自动NAT接入完成。
- 清理：测试Pishoo正常退出、撤回完成，上游已停。停止发布进程后恢复原35.78.0.4:20002-113.80.22.156:21527记录，新的线上查询确认恢复。证据保存在target/dns-live/nat-server.log、nat-client-get.stderr、nat-e2e-result.json、nat-restored-query.stdout和proxy-h3-injected.json。

按用户要求，线上验收移到唯一客户端examples/client.rs（pishoo-client example），提供query、publish、probe、serve、nat-get命令；单元测试只验证本地逻辑，不启动线上DNS、NAT探测或打洞。将客户端从tests/support移到examples，不再新增第二个验收example。example使用标准日志记录实际握手身份、打洞和已验证路径，响应正文与日志分开。生产结构、字段与接口保持不变。qtls 12项测试、qconnection真实连接回归和ddns租期兼容测试通过；构建与最终格式检查另见本次结果。

### 2026-10-03 普通启动自动 NAT 与持续心跳

用户批准为 dhttp 私有 Binding 增加 `nat_probe`，并明确 NAT 分类为每个新 socket 的一次性操作、STUN 绑定心跳持续维护。普通 `DhttpNetwork::init()` 的初始扫描现在装配探测流，由唯一维护任务推进，启动不等待 STUN；Loopback 与 IPv6 link-local 不探测。分类完成后每20秒向同地址族 STUN 节点发送绑定心跳，分别维护映射，登记实际 QUIC 直接/中介别名并更新 AddressBook，已有 Pishoo DNS 维护消费地址变化。分类失败不重新分类，心跳仍继续；不伪造 NAT 类型。心跳失败撤回对应映射，后续心跳继续尝试。绑定撤回和维护任务退出先丢弃流、取消 transaction，再撤回地址及 socket，不存在独立探测任务回写旧地址。

客户端 example 的 `probe/serve/nat-get` 改为等待并检查 Network 维护的结果，删除同 socket 上的第二轮手工分类及映射装配。冻结清单、相邻 Network 详细设计和 README 已同步；DhttpNetwork、Endpoint 与 h3x 成员不变。

验证：dhttp 全部43项库测试通过，包含新增的一次性分类与连续心跳、映射替换/超时撤回、别名冲突保护、分类失败后仍保活以及绑定移除后取消。新增测试使用本地脚本化 STUN 响应，不访问线上节点。dhttp 库 `cargo clippy -- -D warnings`、Pishoo `cargo check --all-targets` 及客户端 example 编译通过。未重跑公网端到端或长期保活验收；此前的公网验收记录保持历史范围。

### 2026-10-03 双身份普通启动与公网续报修复

按用户指定使用 alice.smith.dhttp.net 和 code.alice.smith.dhttp.net，同一普通 Pishoo 进程从 target/workspace-chat-live/home 加载两个身份，各使用隔离的 config/access/workspace/chat 数据库，listen=3。凭据从本地 profile 复制，私钥权限保持0400；身份与 DDNS OCSP 均重新取得并验证。客户端 example 的 run 仅直达现行 pishoo::run，以便日志诊断；最终进程用 target/debug/pishoo 普通入口运行。用户随后要求自行测试，自动联系人和消息步骤已停止，未完成双向 Chat 验收。

用户实际 HTTP 地址查询返回404，genmeta curl 对相同 API 也得到HTTP/3 404（error code1217）；直接 Workspace 管理 API 则为HTTP/3 200。实测旧 DDNS 发布200缺租期头，客户端临时返回300秒，而SIGHUP立即重新发布可恢复H3记录。原Pishoo按100秒续报超过服务端30秒存储租期。publish方法体改为 started + min(lease, MIN_PUBLISH_LEASE) / 3，保持既有300秒兼容返回，实际每10秒续报；不新增成员、配置或任务。重编译并重启同一个双身份普通进程。使用genmeta curl、按API已有允许Origin查询两个身份，复核记录跨30秒存储窗口持续可见；证据保存在 target/workspace-chat-live/renewal-check.json。

### 2026-10-04 中转 DNS 地址修复

用户指出中转 E-record 应为 outer-agent。根因是 Network 已登记 Mediate QUIC 别名，却将其公网映射写成 AddressBook 的 Direct 地址，外部地址表也拒绝 Mediate，导致 DNS 丢失 agent。修复仅改变现有方法体/局部无状态算法：外部地址表允许有效中转地址，内部仍只允许 Direct；FullCone 发布 Direct，受限/未知 NAT 发布各节点的 Mediate，保留实际 socket 的 Direct 别名供打洞。映射替换及撤回按完整 agent/outer 对处理；结构、字段、签名、错误变体和 h3x 不变。DNS 编码器原本就支持 NAT 标记及 outer-agent，不改协议格式；新增 IPv4/IPv6 编码回归。

验证：AddressBook 24项、Network 11项及 DNS packet 7项定向回归通过；Pishoo 普通入口重编译并重启。使用 genmeta curl 对两身份的公网 API 连续4轮、跨度38.2秒查询，8次均200且全部 E-record 为 outer-agent，例如 113.80.22.156:22808-44.253.170.203:20002；证据在 target/workspace-chat-live/relay-dns-result.json。genmeta nslookup --anonymous <name> h3 同样查到两身份的6条中转记录；它的内部 EndpointAddr Display 为 agent-outer，与 DNS API 文本顺序不同，未改变该内部语法。普通进程保持运行供用户测试，用户自己的联系人及 Chat 操作仍由用户完成。

### 2026-10-08 DNS 固定20秒续期

按用户要求，发布成功后统一返回请求开始时间加20秒，不再检查最低租期或按租期计算间隔。失败或超时仍5秒后重试，空地址仍停止发布。同步当前设计说明，不改变结构、字段、函数签名或 ddns 缺头兼容。现有 DNS 库测试4项通过，dns.rs 格式检查通过；未重启运行中的进程或重跑公网验收。

# feat/pishoo-integration 双 VM 回归

原始验收证据仅保留在本地 `vm-evidence/2026-10-09/integration/`，不纳入版本管理。下文证据文件名均相对此目录。

2026-10-09 15:46–15:57（Asia/Shanghai），在 Alice、Bob 两台独立 Linux VM 中，
重新部署当前分支及未提交修改，实际操作 Workspace 并验证双方 DHTTP 交互。
本轮通过的范围是联系人、审批、聊天、访问规则及管理入口读取；没有发现新的产品功能失败。
这份报告不能作为 Note、Lib 安装移除、WebSocket 或公网连接的完整验收结论。

## 实际部署的源码

| 项目 | 值 |
| --- | --- |
| 分支 | `feat/pishoo-integration` |
| HEAD | `4856e259a7c818a5c93bc8e10173f4fed4bd2f5c` |
| 未提交补丁 SHA256 | `5fb97fda201985255121618b4693db33b1130edb260d3df9802a673177de9e37` |
| 前端资源 | `index-D36Bb6Js.js`，两端一致 |
| 工具链 | Rust 1.97.1、Bun 1.4.2 |

源码通过 `git archive HEAD` 和普通工作区补丁导出到 Alice VM 的
`/opt/pishoo/src-integration-20261009`，复用 `/opt/pishoo/src/target`。
依赖使用当前 [锁定快照](../testing-snapshot.md)，没有使用 Mac 父目录的 Cargo patches。
部署包含迁入的消息随机 ID、深链接、主动刷新、聊天分页/轮询/高度和访问规则管理修改。
结束时核对产品源码摘要，与部署快照相同。

只为运行本轮环境定向构建三个目标，耗时 36.58 秒，没有运行 `cargo test` 或全 workspace 构建：

```sh
export PATH=/home/tester/.bun/bin:/home/tester/.cargo/bin:$PATH
export CARGO_BUILD_JOBS=6
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_DEV_INCREMENTAL=false
export CARGO_TARGET_DIR=/opt/pishoo/src/target
cargo build --locked -p pishoo --bin pishoo \
  --example pishoo-client --example vm-workspace-bridge
```

构建自动生成并嵌入当前 Workspace。浏览器桥只加入 VM 源码副本，不修改仓库 targets；
服务、客户端和桥同时升级，避免混用旧 transport。

| 程序 | 两台 VM 的 SHA256 |
| --- | --- |
| `/opt/pishoo/pishoo` | `4978b6f952497b96f7401a90a161a4be4d5e6c7a9a4e163770153c13e355f1e2` |
| `/opt/pishoo/pishoo-client` | `020ae8afa8397311fee3f5105152b48041bed3f307d14ba0c5bc0b1880a966be` |
| `/opt/pishoo/vm-workspace-bridge` | `71e406896e59b1a04cfa1195ff39a767c9834fc397d0e0e6fb9a415dfe365e71` |

`pishoo.service` 继续通过 `pishoo-client run` 启动当前 Pishoo 库。
主二进制本轮执行管理读取命令；无参数启动时写入 `error.log` 的行为未单独验收。

## 环境与认证

沿用 [VM 环境说明](VM_TESTING.md) 的 shared vmnet 局域网：Alice 为 `192.168.76.11`，
Bob 为 `192.168.76.12`，Mac 为 `192.168.76.1`。两个独立 profile 分别使用用户提供的
证书密钥、SQLite 数据库和服务进程，`listen=1`。

页面通过 `18081/18082` 的 VM 浏览器桥访问自己的 Pishoo；桥要求真实 HTTP/3 响应，
证据中的 `transport: HTTP/3` 来自该检查。双方申请、授权确认和聊天投递由实际 worker
通过 QUIC/mTLS 完成；访问审批的业务请求由对端 VM 原生客户端发送。
页面使用 Playwright/CDP 操作实际 AnySee 窗口，没有 mock API、伪造 Visitor 或 SQL 写入审批决定。

两份 OCSP 响应重新获取并验证，均为 `good`、`Response verify OK`；有效窗口分别为
07:47:33–10:47:33 UTC 和 07:47:35–10:47:35 UTC。没有为测试手动关闭 OCSP 校验。
认证记录见 ocsp-validation.json（`ocsp-validation.json`）。

## 通过的流程

| 场景 | 实际结果 | 证据 |
| --- | --- | --- |
| 升级后读取历史与授权 | 原有两端各 128 条消息可读，双方授权有效，互访公开资料 HTTP/3 200 | Alice 基线（`00-alice-baseline.json`）、Bob 基线（`00-bob-baseline.json`） |
| 好友申请送达、拒绝与重复 | Alice 提交 202；Bob 收到并在页面拒绝 204；Alice 同步 `denied`；重复待处理申请 409 | 拒绝与重复（`14-chat-application-denied-and-duplicate.json`） |
| 删除申请与取消未送达申请 | 已拒绝记录删除 204；Bob 停服时取消 queued 申请 204，恢复后没有收到该申请 | 删除与取消（`15-outgoing-delete-and-cancel-queued.json`） |
| 双方同时申请、分别批准 | 两边页面分别批准，Alice 申请 id=13、Bob id=4 均为 `active`，双方 `can_send/can_receive/remote_grant=true` | Alice（`16-alice-simultaneous-active.json`）、Bob（`16-bob-simultaneous-active.json`） |
| 联系人重建保留历史 | 测试中通过页面删除并重建双方联系人，原 128 条消息 ID 全部保留 | 历史核对（`01-lifecycle-history-preserved.json`） |
| 并发与长消息 | 双方共 24 条并发消息及一条 4000 字消息全部投递；每端本轮 25 条消息无重复，发送方 sent、接收方 received | 投递及分页（`13-chat-over-100-history.json`） |
| 消息输入校验 | 空文本、空白、4001 字、NUL 控制字符均返回 400；中文、emoji、换行正常，script 文本未执行 | 输入校验（`11-chat-validation-and-long-queued.json`） |
| 完整历史与实时聊天 | 100+53 条 API 分页完整；两端页面各渲染 153 条，随后 UI 双向发消息自动显示并收发完成 | Alice 页面（`13b-alice-pagination-fixed.json`）、Bob 页面（`13b-bob-pagination-fixed.json`） |
| 对端重试幂等 | 原生客户端重复及三个并发重试返回同一个消息 ID；同 ID 不同文本 409，Bob 仅存一份 | 幂等（`18-native-message-idempotency.json`） |
| 撤权与恢复 | Bob 页面撤权后 Alice 消息 blocked，Bob 未收到；恢复 grant 并主动检查已激活申请后，原 ID 消息送达一次 | 撤权恢复（`20-revoke-and-restore.json`） |
| 离线排队与上线重试 | Bob 停服后记录 `attempt_count=1/remote request timed out`；启动后原消息自动 sent，接收一次且历史完整 | 排队（`21-offline-queued.json`）、恢复（`21-offline-recovered.json`） |
| 两端手动访问规则 | 页面添加 allow、编辑 deny/review、删除，真实对端响应依次 200/403/202；支持根路径、按主体视图和编辑身份锁定 | Alice（`rules/02-alice-manual-rule-real-effects.json`）、Bob（`rules/02-bob-manual-rule-real-effects.json`） |
| 规则错误与权限边界 | 不存在的联系人返回 400，草稿和错误提示保留；非 owner 修改规则 403；中英文入口可用，原规则树恢复 | Alice 收尾（`rules/04-alice-final.json`）、Bob 收尾（`rules/04-bob-final.json`） |
| 双向访问审批允许/拒绝 | 双方各验证 pending→allowed/denied；允许后真实业务上游 200，拒绝后 403；另一身份读取审批状态 404 | 审批证据目录（``） 中 `access-*` 响应及截图 |
| 深链接与聊天滚动 | 联系人、申请联系人和聊天深链接直接访问及刷新 200；阅读历史的两次轮询位置不变，在底部时保持底部 | Alice（`30-alice-deep-links-and-scroll.json`）、Bob（`30-bob-deep-links-and-scroll.json`） |
| 聊天高度 | 1440×960、768×1024、375×812、375×667 均无外层页面溢出，输入框可见且不重叠底部导航 | 尺寸记录（`30-chat-layout-matrix.json`） |
| 应用与管理读取 | Apps 页显示未加载应用；owner catalog 200、对端 403；CLI listen/proxy/lib list/lib --loaded 及 `/pishoo/settings`、`/pishoo/proxies`、`/pishoo/libs` 可读 | Alice（`31-alice-apps-and-cli.json`）、Bob（`31-bob-apps-and-cli.json`）、最终状态 |

访问审批的 `/vm-review/` 代理会去掉这个前缀，因此真实上游响应中的 `path` 为余下路径。
原生幂等测试直接在 Bob 接收侧新增一条消息，没有在 Alice 发送队列新增对应记录；
最终 Alice 158 条、Bob 159 条是这个场景的预期差异。

## 收尾状态与异常记录

两台 VM 的 `pishoo/browser-bridge/test-upstream` 均 active，当前二进制摘要一致，
双方 Chat 授权有效，所有本轮消息已完成收发，没有遗留 pending 审批或临时 ACL 规则。
原 128 条历史逐条核对 ID、client_message_id、文本和创建时间，完整保留。
八个 SQLite 数据库的只读 `PRAGMA integrity_check` 均为 `ok`。
Bob 对 `spike.liu` 的消息授权仍存在；本轮没有操作 Mac 日常 profile。

浏览器没有记录到 pageerror。记录的三个 502 出现在为取消申请和离线重试主动停止 Bob 的期间；
恢复后相关读取及消息投递通过。运行日志仍包含连接 idle timeout/ClosedCriticalStream
和 NAT/STUN 诊断，本轮不据此宣称此前 HTTP/3 异常或公网问题已解决。

验收脚本曾将首页标题误写为“概览”，以及把返回申请 JSON 的 refresh 误断言为 204。
修正为实际“工作台”和 200 后完成对应场景；这两次是验收脚本错误，没有改产品源码。
撤权场景因此执行了两次，最终状态检查确认两次消息均已恢复。

完整结果见 summary.json（`summary.json`）、
Alice 最终状态（`40-alice-final-state.json`）、
Bob 最终状态（`40-bob-final-state.json`）。

升级前停止服务并备份了旧三个程序、四个数据库和 OCSP，保留在各 VM 的
`/opt/pishoo/backups/integration-20261009-before/`。
本轮没有暂存、提交、改分支或修改产品源码；只新增证据和更新验收文档。

## 本轮没有覆盖的范围

- 两台 VM 均未安装 Lib，应用目录为空；没有实测真实 Note component、Lib 安装/更新/移除、逐 Lib 数据隔离或加载后的代理路由竞争。
- WebSocket 代理及完整 Home Assistant 资源加载没有做本轮双 VM 专项验证。
- 管理命令本轮只验证读取，没有在停服状态下验证离线写入或服务管理命令。
- 公网/NAT、同事的实际身份与网络、证书轮换、AnySee 自身原生 DHTTP 证书导入不在本轮范围内。
- 未重跑整套前端 E2E、七天申请过期时钟场景或完整 VM 重启；这些历史记录仍见旧基线报告。

## 证据与保留环境

源码、构建日志、场景驱动脚本和二进制包保留在仓库外：

```text
/Users/x/Library/Caches/pishoo-vm-acceptance/20261009-integration/
  source.tar / source.patch
  prepare.py / deploy.py / refresh-ocsp.py
  common.mjs / baseline.mjs / lifecycle.mjs / chat.mjs / chat-ui.mjs
  recovery.mjs / rules.mjs / access.mjs / layout-routes.mjs / final.mjs
  build.txt / runtime.tar.gz / evidence/
```

仓库证据位于 integration/（``），包含源码摘要、部署摘要、
实际响应、页面截图及 JSONL 运行日志，不随代码提交；不包含证书私钥、SSH 私钥、数据库或 VM 磁盘。
两台 VM 与浏览器继续运行，可从 [Alice](http://127.0.0.1:18081/workspace/) 和
[Bob](http://127.0.0.1:18082/workspace/) 查看当前状态。

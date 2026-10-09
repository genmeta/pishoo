# 2026-10-08 双 VM Workspace 实测报告

原始验收证据仅保留在本地 `vm-evidence/2026-10-08/`，不纳入版本管理。下文证据文件名均相对此目录。

已实际启动 Alice、Bob 两台 Linux VM，使用用户提供的不同证书和独立数据库，完成双方
加好友、授权、审批和聊天。修复后的一轮交互流程通过；测试期间仍出现 HTTP/3 响应流异常，
因此本报告不代表传输稳定性已完成验收。

本报告保留首轮快照。随后已继续补测下文的覆盖缺口，并发现及修复并发消息 ID 冲突和
超过 100 条历史时的新消息遗漏；最新结果与最终运行状态见
[补充实测报告](VM_EXTENDED_ACCEPTANCE_2026-10-08.md)。

## 环境与证据

- 代码基线：`feat/rebase-daccess` / `c57c08d1e00272e5d0a8ccd73c7460e65c84a12a`，已核对远程分支。
- 实测时间：2026-10-08，约 16:30–17:31，Asia/Shanghai。
- VM：Ubuntu 24.04 ARM64、QEMU 11.1.1、HVF 加速，两套独立磁盘和进程。
- Alice：`192.168.76.11`，身份 `alice.forlocaltest.dhttp.net`。
- Bob：`192.168.76.12`，身份 `bob.forlocaltest.dhttp.net`。
- 身份发现使用 mDNS。日志还确认了两台 VM 之间的 IPv6 link-local QUIC 路径。
- 两个独立 AnySee 窗口实际操作 Workspace；浏览器通过 VM 内的原生 DHTTP 客户端访问各自 Pishoo。
  双方 contact/chat worker 直接通过真实 QUIC/mTLS 互通。没有使用 API mock、伪造身份头或伪造 Visitor。
- 当前运行包含本轮未提交的修复，精确改动见 tested-fixes.patch（`tested-fixes.patch`）。

环境摘要、镜像摘要、证书序列号和二进制摘要见 environment.json（`environment.json`）。
首轮采样时两端运行的相同二进制摘要：

| 程序 | SHA-256 |
| --- | --- |
| `pishoo` | `f8c2422ea049b9cc05904bd10efe2e1aa59a6784a34c58151e6f1857de17d19b` |
| `pishoo-client` | `aa96df910449ba41e751d7b6f1c0088bf1a27c2300f106728041ec1ea5545c3f` |

服务实际使用 `/opt/pishoo/pishoo-client run` 调用同一 `pishoo::run()`，以便记录 transport 日志。
只在 VM 内局部构建运行所需程序；本轮没有用 `cargo test` 代替真实操作。

## 实际结果

| 场景 | 实际行为 | 结果 |
| --- | --- | --- |
| Alice 加 Bob | 页面选中请求及提供 Chat；本地 `202/queued`，真实 worker 向 Bob 投递 | 通过 |
| Bob 审批 Chat | 审批中心显示 Alice，点击允许返回 204；Alice 申请变为 active | 通过 |
| 双方授权确认 | 两端 `can_send=true`、`can_receive=true`、`remote_grant=true` | 通过 |
| Alice 发给 Bob | 本地 202，发送方 sent，接收方 incoming/received；无需刷新页面 | 通过 |
| Bob 回复 Alice | 同样经过后台 QUIC 投递；无需刷新页面 | 通过 |
| Bob 允许 Alice 访问 | 原请求 202，审批后状态 allowed，重试 200，实际到达 Bob 的业务上游 | 通过 |
| Bob 拒绝 Alice 访问 | 原请求 202，审批后状态 denied，重试 403 | 通过 |
| Alice 审批 Bob 访问 | 交换申请方和审批方，重试到达 Alice 的业务上游并返回 200 | 通过 |
| 审批状态身份限制 | 发起方查状态 200，另一身份查同一状态 404 | 通过 |
| Bob 撤销 Chat | Alice 消息被远端 403 阻塞；Bob 没有收到该消息 | 通过 |
| Bob 恢复 Chat | 页面授予返回 204；Alice 点击检查状态，原消息恢复且只接收一份 | 修复后通过 |
| Bob 服务离线再上线 | Alice 202 入队，worker 记录重试；上线后同一消息 sent/received，历史保留 | 通过 |
| 离线期间的聊天页面 | 请求失败后显示错误，服务恢复后自动显示收到的消息 | 修复后通过 |
| 联系人详情直接访问 | 完整 DHTTP 名称的详情路径返回 200；不存在的 JS asset 仍返回 404 | 修复后通过 |
| 聊天滚动位置 | 阅读历史时连续两次刷新保持位置；底部跟随消息，最新消息位于实际可见区域 | 通过 |
| 聊天页高度 | 桌面、平板和手机宽度下填满剩余主内容区；页面无额外滚动，标题和输入框可见 | 修复后通过 |

最终一轮双方新消息自动显示耗时约 **3.20 秒、7.61 秒**，并在页面上观察到发送状态变为「已发送」。
耗时包含后端投递与前端 3 秒刷新周期，不能用作公网性能结论。

撤权恢复复验使用原消息 `id=13`、`client_message_id=1791450329-2`，没有创建替代消息。
恢复授权并检查状态后约 2 秒投递成功，Bob 只收到一份。离线复验使用消息 `id=14`，重试后
Bob 的会话历史从 13 条增加到 14 条，新增消息只有一份。

首轮最终数据库统计：Alice 有 11 条 outgoing/sent 和 5 条 incoming/received；Bob 有 5 条
outgoing/sent 和 11 条 incoming/received。两端联系人均为 active，四个数据库权限为 0600，
私钥权限为 0400。见 Alice 运行状态（`alice-runtime-final.txt`） 和
Bob 运行状态（`bob-runtime-final.txt`）。

## 首轮覆盖缺口（已继续补测）

首轮验证了上表中的双人交互主流程。以下表格保留首轮未完整覆盖的项目；
这些项目的后续补测方式、结果和剩余限制见[补充实测报告](VM_EXTENDED_ACCEPTANCE_2026-10-08.md)。

| 已实现的功能或边界 | 首轮尚未覆盖的场景 |
| --- | --- |
| 联系人及能力申请 | 拒绝 Chat 申请、删除发出的申请、申请过期、重复申请及双方同时申请 |
| 联系人管理 | 拉黑与解除拉黑、别名、本地保存与取消保存、分类筛选、分页和批量删除 |
| 审批时效与记录 | 一次性授权后的第二次访问行为、临时授权到期、过期审批查看与清理 |
| 身份和管理边界 | 非 owner 访问 Workspace/Chat 管理 API、SubjectId 变化后旧任务的处理 |
| 个人资料 | 修改显示名称、上传或删除头像、对端读取更新后的公开资料 |
| 聊天规模与输入边界 | 历史超过 100 条、长消息和长度限制、连续发送及并发投递 |
| 设置展示 | 访问规则及能力目录页面的完整操作验收 |

首轮只验证了服务停止再启动。补测已经完成两台 VM 的完整内核重启；长期运行尚未验收。
API 扩展与应用管理目前仍是占位页面；群聊和消息已读也不属于当前已验收的功能。

## 本轮发现并修复的流程问题

1. **已收到消息需要手动刷新。** 原聊天页没有刷新消息列表，两边都复现：数据库已 received，
   页面 8 秒内仍不显示。聊天页现在在可见时每 3 秒刷新消息和能力，返回页面时立即刷新；
   在底部阅读时跟随新消息，阅读历史时保留滚动位置。双向页面复验通过。
2. **连接失败后会话不能自动恢复。** 异步 resource 处于错误状态时，派生状态继续读取 resource，
   造成未处理错误。聊天可用性避开错误状态的 resource，移除会在错误状态中读取消息的派生状态，离线上线复验中页面自动恢复。
3. **带完整身份的联系人详情直接访问 404。** 服务端把 `alice.forlocaltest.dhttp.net` 的 `.net`
   当作文件扩展名，未返回 SPA 入口。现在识别联系人详情路由，并继续让缺失静态资源返回 404。
4. **已激活申请的「检查状态」没有实际刷新。** 接口仅更新 queued/pending，active 直接返回，
   恢复 Chat 时只能等待默认 300 秒轮询。现在 active 也会设置检查时间并唤醒真实 worker；
   最终复验中点击按钮后原 blocked 消息恢复。
5. **聊天页没有填满高度，整页还会滚动。** 主内容区沿用首页浮动聊天框预留的 188px 底部留白，
   聊天列表又使用固定数值扣减高度；手机布局还取消了列表最大高度。现在聊天路由使用视口高度，
   主内容区去除那段预留空间；聊天页和会话框按剩余高度伸缩，仅消息列表内部滚动。
   手机端为底部导航保留空间。两端在 1440×960、768×1024、375×812 和 375×667 下复验。

聊天刷新还调整了滚动行为：响应返回后记录位置，列表更新后的动画帧恢复位置；用户在底部时
继续跟随最新消息。复验还发现若在请求发出前记录位置，用户在请求期间滚动会被旧位置覆盖，
因此最终实现改为响应返回后记录，并关闭消息列表的原生滚动锚定，避免与显式位置恢复互相影响。
最终滚动复验使用独立的真实 Workspace 标签页，连续两次刷新保持历史位置；
回到底部后连续两次刷新均保持在底部，最新消息实际可见。

涉及 `pishoo/workspace/src/pages/ChatPage.tsx`、`pishoo/workspace/src/App.tsx`、
`pishoo/workspace/src/index.css`、`pishoo/src/routes/access.rs` 和
`pishoo/src/workspace/contacts.rs`。修复已在两台 VM 中实际运行；VM 的 Bun 构建包含 TypeScript
检查，修改文件的局部格式检查与 `git diff --check` 通过。没有暂存或提交这些改动。

另外，部署时确认 profile 目录名必须是 `alice.forlocaltest` / `bob.forlocaltest`，需要去掉
`.dhttp.net` 后缀。早期方案中的完整名称目录写法已经在 [VM_TESTING.md](VM_TESTING.md) 修正。

## 仍需定位的实际异常

测试期间发生过 HTTP/3 `ExcessiveLoad`，响应流被以应用错误码 **263** 重置，转发入口返回 502。
实际涉及静态 JS、Workspace context/profile，以及一次恢复 Chat 的 POST。
日志还出现 `QPACK instruction queue is full`，这是后续排查 HTTP/3 实现的线索。

其中一次 POST 的 Pishoo 服务端日志已记录 204，客户端却没有得到正常响应，提示操作结果存在
不确定性。原生新客户端重查授权后确认已生效；最终复验的页面授权正常返回 204。重建连接后
能够继续操作，但尚未定位根因，不能称作已修复，也不能直接把问题归因于 Workspace 或测试桥。
证据见 HTTP/3 异常上下文（`http3-response-error.log`）。

另一个文案问题是消息状态 blocked 复用了联系人「已拉黑」文案；本次撤权时联系人实际上仍为
active，含义应是远端未授予投递能力。此处保留现状，报告区分联系人状态与消息状态。

## 截图和关键响应

- Alice 填写真实好友申请（`02-alice-contact-request.png`）
- Bob 页面收到 Chat 审批（`05-bob-capability-approval.png`）
- Bob 自动收到 Alice 消息的可见区域采样（`23-alice-automatic-ui-receive.json`）
- Alice 自动收到 Bob 回复的可见区域采样（`24-bob-automatic-ui-receive.json`）
- Bob 处理访问允许（`access-alice-allow-pending-ui.png`）
- Bob 处理访问拒绝（`access-alice-deny-pending-ui.png`）
- Alice 页面显示投递被阻塞（`31-alice-blocked-ui.png`）
- Bob 收到恢复授权后的原消息（`34-bob-restored-message-ui.png`）
- Alice 最终聊天视口（`41-alice-scroll-fixed-ui.png`）
- Bob 最终聊天视口，含离线上线后收到的消息（`41-bob-scroll-fixed-ui.png`）

最终可见区域和滚动采样见 41-final-browser-state-scroll-fixed.json（`41-final-browser-state-scroll-fixed.json`）。
截图复核确认 Playwright 的 `fullPage` 采集会将内部聊天容器置顶，而普通视口截图保留位置。
旧 40-final-browser-state.json（`40-final-browser-state.json`） 和旧全页截图
保留作为采集过程记录；其中消息不可见的采样来自截图后的状态，不能据此判断聊天轮询存在回顶问题。
最终截图使用普通视口采集，且在采集后再次确认滚动位置。
采集影响见 42-screenshot-scroll-impact.json（`42-screenshot-scroll-impact.json`）。

聊天页高度的实际 DOM 测量见 43-chat-viewport-layout.json（`43-chat-viewport-layout.json`），
包括文档高度、可见区域、输入框及底部导航位置；
对照 桌面截图（`43-alice-1440x960-chat-layout.png`） 和
手机截图（`43-alice-375x812-chat-layout.png`）。

申请响应、审批状态、双方能力、消息状态和重试记录保存在同目录的 JSON 文件中。
真实 H3 请求日志见 Alice（`alice-http3-interactions.log`） 和
Bob（`bob-http3-interactions.log`），包含服务端身份、经过 TLS 验证的 peer、
HTTP/3 版本、路径和状态码。

## 继续操作

两台 VM 和两个独立浏览器保留运行：

- Alice：<http://127.0.0.1:18081/workspace/>
- Bob：<http://127.0.0.1:18082/workspace/>

复用方法、SSH 入口、证书路径和拓扑见 [VM_TESTING.md](VM_TESTING.md)。完整日志、浏览器编排
脚本、转发辅助程序和 VM 启动参数位于本机仓库外的
`/Users/x/Library/Caches/pishoo-vm-acceptance/20261008/`。
本地证据不含私钥或 VM 磁盘；用户原有 `anysee-demo/` 和 zip 保留。

本轮覆盖虚拟局域网中的双人流程；没有验证公网 NAT 穿透、AnySee 原生证书导入/选择、
证书轮换、第三人访问、群聊或消息已读。

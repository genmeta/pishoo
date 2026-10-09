# Pishoo / DHTTP 双 VM 真实环境验收

原始验收证据仅保留在本地 `vm-evidence/2026-10-09/`，不纳入版本管理。下文证据文件名均相对此目录。

这套环境用于实际操作 Workspace：Alice、Bob 分别运行在独立 Linux VM 中，使用各自证书，
完成加好友、双方授权、访问审批和双向聊天。实测结果见
[首轮验收报告](VM_ACCEPTANCE_2026-10-08.md)和
[补充实测报告](VM_EXTENDED_ACCEPTANCE_2026-10-08.md)；补测含并发、大历史、申请过期及完整 VM 重启。
10 月 9 日进一步恢复访问规则管理入口并实测权限效果，见[规则管理报告](VM_RULE_MANAGEMENT_2026-10-09.md)。
同日把 Mac 接入共享虚拟局域网，完成 `spike.liu` 与 Bob 的真实交互，见
[本机与 VM 验收报告](VM_HOST_ACCEPTANCE_2026-10-09.md)。
迁入当前集成分支后，又重新部署程序和浏览器桥完成
[feat/pishoo-integration 双 VM 回归](VM_INTEGRATION_ACCEPTANCE_2026-10-09.md)。

## 代码与运行环境

首轮基线为远程 `feat/rebase-daccess`，提交 `c57c08d1e00272e5d0a8ccd73c7460e65c84a12a`。
当前两台 VM 运行 `feat/pishoo-integration` 的 `4856e259a7c818a5c93bc8e10173f4fed4bd2f5c`
及迁入的未提交修复，实际部署摘要和通过范围以集成分支回归报告为准。
当前服务使用 `DHTTP_HOME`、四个 SQLite 数据库和进程内 worker；旧 `server.conf`、
独立 worker supervisor 不适用于这个分支。实际验收发现的修复另列在报告中。

宿主机是 Apple Silicon macOS；两台 VM 使用 QEMU 11.1.1、HVF 加速和 Ubuntu 24.04 ARM64
cloud image。每台 VM 配置两块网卡：第一块通过 QEMU user networking 出站，第二块接入
双方共用的虚拟局域网。两个独立的 user networking 实例不能直接作为双方通信的局域网。

| 项目 | Alice | Bob |
| --- | --- | --- |
| 身份 | `alice.forlocaltest.dhttp.net` | `bob.forlocaltest.dhttp.net` |
| 主机名 | `pishoo-alice` | `pishoo-bob` |
| 内网地址 | `192.168.76.11/24` | `192.168.76.12/24` |
| SSH 入口 | `127.0.0.1:22211` | `127.0.0.1:22212` |
| Workspace 入口 | <http://127.0.0.1:18081/workspace/> | <http://127.0.0.1:18082/workspace/> |
| AnySee CDP | `127.0.0.1:19181` | `127.0.0.1:19182` |
| VM 资源 | 8 vCPU、16 GiB，兼作构建机 | 2 vCPU、4 GiB |

当前共享局域网使用 macOS `vmnet` 的 shared 模式，经 `socket_vmnet` 接入两台 QEMU。
Mac 的 `bridge100` 地址为 `192.168.76.1/24`，Alice、Bob 沿用 `.11`、`.12`。
两台 VM 的 LAN 参数为 `-netdev socket,id=lan,fd=3`，由 `socket_vmnet_client` 传入连接。
这是承载以太网帧的 VM 网络连接；业务请求仍由 DHTTP 通过真实 UDP/QUIC 传输。
身份发现使用当前程序自带的 mDNS，包含实际 QUIC 端口，无需假设监听固定在 UDP 443。
出站网卡用于获取 OCSP 和安装工具；本次运行配置 `listen=1`，验收范围为虚拟局域网。

### 宿主机访问与公网连接的边界

**以下为切换共享网络之前的排查记录。** 原网络只把 Alice、Bob 相连：Alice 监听
`127.0.0.1:22760`，Bob 连接该端口。Mac 没有接入，无法参与其中的 mDNS 身份发现。
`127.0.0.1:18081`、`:18082` 仅转发浏览器入口的 TCP；原生 DHTTP 使用的 UDP/QUIC
没有通过这些端口转发。因此，宿主机的 `spike.liu` 直接向 `bob.forlocaltest` 发送申请，
可能停在名称解析阶段并保持 `queued`，不能用浏览器入口可打开来判断这条连接可用。

2026-10-09 13:44–13:49（Asia/Shanghai）的只读排查确认：

- Bob 服务 active，`db/config.db` 中 `listen=1`；Alice 原生 HTTP/3 读取 Bob 的公开资料返回 200。
- H3 DDNS 查询 Bob 返回 `no DNS record found`。Bob 当前只公布虚拟局域网身份，没有公网记录。
- 宿主机的 `spike.liu` 已运行且 `listen=3`；H3 DDNS 能查到其公网候选地址。
- Alice、Bob 分别使用 `nat-get`，等待自己的公网 NAT 映射并排除内网路径后，读取
  `https://spike.liu.dhttp.net/std/profile` 均失败，错误为
  `DNS lookup for spike.liu.dhttp.net ended without a usable path`。
  这是宿主机与 VM 之间通过公网候选地址建立连接的失败，尚未完成 QUIC 握手或好友审批。

采样见宿主机与 VM 连接排查（`host-vm-network-diagnosis.json`）。
这次没有修改用户日常数据库、VM 监听范围或网络拓扑，也没有解决公网路径问题。
公网候选地址存在不等于连接可用；这项失败也不能直接证明同事此前失败的具体原因。

随后已切换到上述 vmnet 共享网络，正常关闭并重新启动两台 VM，再恢复本机 Pishoo。
本机已有的 Bob 申请 `id=7` 自动送达，经 Bob 页面审批变为 `active`，双方聊天通过。
服务日志验证实际路径包含 `192.168.76.1` 与 `192.168.76.12`，详见
[本机与 VM 验收报告](VM_HOST_ACCEPTANCE_2026-10-09.md)。

公网和强制 OCSP 的排查按用户要求留待后续。跨网交互需要单独检查公网发布、NAT 路径及真实收发；
仅把 `listen` 改为 `3` 或配置 TCP 转发不能作为通过依据。

## 证书与 profile 布局

用户提供的凭据：

| 身份 | 证书链 | 私钥 |
| --- | --- | --- |
| Alice | `/Users/x/Downloads/alice.forlocaltest.dhttp.net.crt.pem` | `/Users/x/Downloads/alice.forlocaltest.dhttp.net.key.pem` |
| Bob | `/Users/x/Downloads/bob.forlocaltest.dhttp.net.crt.pem` | `/Users/x/Downloads/bob.forlocaltest.dhttp.net.key.pem` |

两份证书链各含 leaf 和 intermediate，支持 TLS 客户端及服务端身份认证，私钥与证书匹配；
有效期为 2026-10-08 至 2027-10-08。源文件留在 Downloads，私钥内容不进入仓库或报告。

**profile 目录名必须去掉 `.dhttp.net` 后缀。** 完整身份名称仍用于请求和证书；当前发现逻辑
会跳过目录名包含完整后缀的 profile。Alice 的实际布局如下，Bob 使用 `bob.forlocaltest`：

```text
/srv/dhttp/alice.forlocaltest/
  ssl/fullchain.crt
  ssl/privkey.pem       # 0400
  ssl/ocsp.der          # 启动获取并验证
  db/config.db
  db/access.db
  db/workspace.db
  db/chat.db
```

两台 VM 都以普通用户 `tester` 运行，设置 `DHTTP_HOME=/srv/dhttp`，不共享证书、数据库或
运行目录。home/profile 目录为 0700，数据库为 0600。证书使用 `fullchain.crt`，私钥使用
`privkey.pem`。初次启动需要 HTTPS 出站以获取匹配证书的 OCSP，不能以占位 DER 文件替代。

## 构建与服务

只在 Alice VM 内构建当前源码的 Pishoo 二进制及实际测试所需的客户端，然后把相同二进制
部署到 Bob。源码用只读 `git archive HEAD` 导出，必须包含根目录 `wit/`；不要用会产生
macOS `._*` 元数据文件的归档方式，以免 WIT 解析失败。

运行依赖 Rust 1.97.1 和 Bun。VM 中的局部构建命令如下；这里的构建是为了运行 Linux
服务和客户端，不运行 `cargo test`：

```sh
export PATH=/home/tester/.bun/bin:/home/tester/.cargo/bin:$PATH
export CARGO_BUILD_JOBS=6
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_DEV_INCREMENTAL=false
cargo build --locked -p pishoo --bin pishoo \
  --example pishoo-client --example vm-workspace-bridge
```

本次运行服务：

- `pishoo.service`：`/opt/pishoo/pishoo-client run`，执行当前库的 `pishoo::run()`，同时启用日志订阅；
- `browser-bridge.service`：在各自 VM 内加载各自证书，将浏览器请求转发给自己的 Pishoo；
- `test-upstream.service`：监听 VM 内 `127.0.0.1:8090`，提供需要审批的业务接口。

浏览器转发辅助程序只添加到 **VM 的源码副本**中，未改变仓库的 Cargo targets。它使用
当前锁定的 `dhttp::Endpoint`，保留方法、路径、请求体及响应状态，并要求上游确为 HTTP/3。
Workspace HTML、静态资源和业务 API 均来自实际 Pishoo；没有 mock 或伪造 Visitor。

```text
独立 AnySee 窗口
  -> 宿主 127.0.0.1:18081 / :18082
  -> QEMU TCP 转发到对应 VM :8080
  -> VM 原生 DHTTP 客户端，以自己的证书连接自己的 Pishoo

Alice Pishoo worker <-> 共享虚拟局域网中的真实 QUIC/mTLS <-> Bob Pishoo worker
```

这套入口验证 Workspace UI 和真实 DHTTP 后端，但不验证 AnySee 浏览器自身的 DHTTP
证书导入、原生连接或证书选择。`user-data-dir` 本身也不能证明隔离了原生 DHTTP 身份。
本次两个 AnySee 进程分别使用独立 browser profile 和独立 `DHTTP_HOME`。

## 实际交互顺序

### 加好友与双方 Chat 授权

1. Alice 的「添加联系人」页面填写 Bob 的完整 DHTTP 名称，同时选择请求和提供一对一聊天。
2. Alice 本地返回 `202/queued`，实际 contact worker 通过 mTLS 向 Bob 提交申请。
3. Bob 的审批中心出现 Alice 的 Chat 能力请求；在页面点击「允许」。
4. Alice 的申请状态变为 `active`；Bob 的 worker 从 Alice 的 `GET /contact/self` 确认反向授权。
5. 两端能力状态均为 `can_send=true`、`can_receive=true`、`remote_grant=true`。
6. 双方分别在聊天页面发送不同消息，确认发送接口先返回 `202/queued`，随后发送方为 `sent`、
   接收方为 `incoming/received`，页面自动显示新消息。

授权方向为：Bob 授予 Alice，允许 Alice 发给 Bob；Alice 授予 Bob，允许 Bob 发给 Alice。
申请中声明的 offers 需要实际对端确认，不能仅凭声明认定已授权。`sent` 表示对端接收，
不表示对方已读。

### 访问审批

通过 owner 的真实 API 为测试接口设置 `review`，例如在 Bob 上：

```http
POST /acl/access/vm-review/alice-allow
Content-Type: application/json

{"method":"GET","effect":"review","grantee":"alice.forlocaltest.dhttp.net"}
```

测试代理映射 `/vm-review/` 到 VM 本地业务上游；代理配置需要在启动前写入或通过
当前分支的 `/pishoo/proxies` 保存后重启（旧基线使用 `/sys/proxies`）。
ACL 规则通过 API 配置，不用 SQL 写入审批决定。

1. Alice VM 的原生客户端请求 Bob 的业务路径，得到 `202` 和 `review_id/status_url`。
2. Alice 使用同一证书查询状态，得到 `pending`；另一身份查询该状态应被拒绝。
3. Bob 在 Workspace 审批中心点击允许或拒绝，并确认对话框。
4. Alice 查询 `allowed/denied`，然后以相同方法、路径、请求头重试业务请求。
5. 允许时应实际到达 Bob 的业务上游并返回 200，拒绝时应返回 403。
6. 交换角色，由 Bob 申请、Alice 审批，验证反向流程。

### 撤权、恢复与离线

- Bob 在联系人详情撤销 Alice 的 Chat 能力，Alice 的后续消息应被远端 403 阻塞，Bob 不收消息。
- Bob 重新授予 Chat；Alice 在已发送申请页面点击「检查状态」，让 worker 提前确认远端授权。
  已激活申请默认每 300 秒检查一次，不能假定恢复授权立即通知到对方。原被阻塞消息应恢复投递，
  `client_message_id` 不变且只收到一份。
- 停止 Bob 的 Pishoo 服务后，Alice 发送消息；恢复 Bob 后确认后台重试、历史保留以及单次接收。

## 保留环境与收集证据

本机运行目录在仓库外：

```text
/Users/x/Library/Caches/pishoo-vm-acceptance/20261008/
  vm_ssh                 # 临时 SSH 私钥，不复制进仓库
  known_hosts
  images/
  alice/                 # 磁盘、cloud-init、launch.json、QEMU pid、独立浏览器目录
  bob/
  evidence/              # 页面截图、响应、状态、日志和修复 patch
```

SSH 示例，Bob 改用端口 22212：

```sh
ssh -i /Users/x/Library/Caches/pishoo-vm-acceptance/20261008/vm_ssh \
  -o UserKnownHostsFile=/Users/x/Library/Caches/pishoo-vm-acceptance/20261008/known_hosts \
  -p 22211 tester@127.0.0.1
```

在 VM 内查看实际服务和日志：

```sh
systemctl status pishoo browser-bridge test-upstream
sudo journalctl -u pishoo --no-pager
sudo journalctl -u browser-bridge --no-pager
ss -lunp
```

每次验收记录源码提交与未提交修复、运行二进制摘要、证书序列号、双端数据库状态和页面截图。
重用 VM 时保留现有历史；需要全新申请场景时先停止服务并备份测试数据库，再重建测试数据。
不得删除日常 `~/.dhttp`，不得把 VM 磁盘或私钥上传到仓库。

双 VM 首轮不涵盖公网 NAT 穿透、证书轮换、第三个身份、群聊和消息已读；后续已加入本机
`spike.liu` 的局域网交互。公网、证书轮换、群聊和消息已读仍不能由本次通过结果推断。

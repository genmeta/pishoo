# Home Assistant HTTP/3 修复提交说明

## 1. 范围与基线

本文记录 `sky.lee~` 代理 Home Assistant 期间在三个本地仓库中的修复。提交按依赖顺序排列，每项分别说明现象、根本原因、方案和验证范围。

- `h3x` 基线：`776cbfb`（`v0.6.0-beta.4`）
- `dhttp` 基线：`c315cb4`，本地包版本 `0.6.0-beta.5`
- `pishoo` 基线：`adbdda3`
- `pishoo` 使用仓库内 `target/release/pishoo`，不使用 `/opt/homebrew/bin/pishoo`
- `dhttp` 和 `h3x` 原有未提交修改已保存在 stash 中，本次整理未删除或展开这些 stash

这些提交中有三类变更：

1. 已确认的行为修复，例如连接池、响应缓冲、流关闭语义。
2. 防御性修复，例如 malformed request stream 的明确失败处理。
3. 诊断能力，例如 qlog 和跨 IPC/QUIC 边界的序号日志。诊断提交本身不声称修复数据丢失。

## 2. h3x 提交

### `645b452 fix(dhttp): expand unresolved request stream queue`

**现象**

Home Assistant 首屏会并发加载大量资源和 API 请求。HTTP/3 request stream 在完成协议分类后、被上层消费前会短暂堆积，原有 32 项 ring capacity 很容易在 burst 中触顶。

**根本原因**

队列使用淘汰最旧元素的 ring 语义。容量不足时，合法 request stream 可能在上层接收前被新流替换，表现为请求随机缺失或 Extended CONNECT 等待不到响应。

**修复方案**

把 unresolved request stream capacity 从 32 提高到 256，保留原有有界队列和淘汰策略，只扩大 Home Assistant burst 的容纳范围。

**验证**

协议测试覆盖队列达到容量后的淘汰行为，并确认容量快照为 256。

### `a0dc765 fix(rpc): flush pending frame sends before close`

**现象**

RPC frame 已进入 `start_send`，随后立刻发生 `poll_close` 时，接收端偶发看不到最后一条消息。

**根本原因**

remoc sink 的一次发送不是在 `start_send` 返回时就完成。旧 `poll_close` 可以在已开始的异步发送完成前关闭底层 sink，使最后一个 frame 被截断。

**修复方案**

在关闭前持续推进 pending send future；只有该消息成功送入 remoc sink 后才 flush/close。发送失败则把错误返回给调用方。

**验证**

新增 close/flush 生命周期回归测试，并把 IPC 双向测试扩展为双方各连续发送 16 帧。

### `8a50f79 fix(rpc): keep bridge EOF scoped to its stream`

**现象**

一个 IPC stream bridge 遇到 EOF 后，其他仍健康的 QUIC stream 也可能随整个 connection 一起失败；WebSocket 上表现为单流结束扩大成连接级错误。

**根本原因**

reader/writer bridge 把本地 IPC EOF 转换并写入共享 connection lifecycle。EOF 属于当前 bridge/stream 的终止条件，不足以证明底层 QUIC connection 已损坏。

**修复方案**

把普通 bridge EOF 映射为当前 stream 的 `H3_REQUEST_CANCELLED` reset，不再污染共享 connection lifecycle；真实 transport/connection error 仍保持连接级传播。

**验证**

回归测试确认 IPC writer EOF 只 reset 当前 stream，connection lifecycle 继续可用。

### `e96a02f fix(dhttp): drain request receive side before stopping`

**现象**

request handler 完成或 wrapper 被 drop 时，接收侧可能立即发送 `STOP_SENDING`。对仍在路上的 body/frame，这会过早终止 request stream，并可能与双向 WebSocket 的关闭时序互相放大。

**根本原因**

旧 guard 的 drop cleanup 无条件停止 QUIC receive side，没有先区分正常 EOF 和真正未消费完的长流。

**修复方案**

drop 后启动受限排空：先读取到 EOF；正常 EOF 不发送 `STOP_SENDING`，排空超时或读取失败时再停止 stream。`take()` 仍把 cleanup 所有权转交给新 wrapper，避免旧 wrapper 抢先清理。

**验证**

测试覆盖正常 EOF、超时停止、读取错误，以及 `take()` 后只有最终 owner 执行清理。正常 EOF 的断言明确检查 channel 关闭而不是收到 stop code。

### `78bdfdd fix(dhttp): fail malformed request stream classification`

**现象**

HAR 21-24 期间出现 request stream（例如 stream 80）首个 VarInt 不是合法 HTTP/3 frame type 的记录。旧逻辑会把无法解码首个 VarInt 的流交给下一协议层，之后的错误位置与原始坏前缀脱节，容易表现为超时或协议错误。

**根本原因**

分类器把“合法但不属于 HTTP/3 的 frame type”和“连一个完整 VarInt 都无法读取”视为同一种 `Passed` 结果；同时普通 `io::Error` 被直接转换时可能进入只接受封装 QUIC error 的路径并 panic。对合法未知 frame type，peek cursor 也没有在交给下一层前复位。

**修复方案**

- 完整记录首块长度、十六进制前缀、frame type 和分类结果。
- EOF/非法 VarInt 明确返回 stream-level `H3_FRAME_ERROR`。
- 能恢复出的 QUIC stream error 保留原错误；普通 I/O 解码错误安全映射为 reset。
- 合法但未知的 frame type 仍交给下一协议层，并先复位 cursor。
- 测试数据改用正确的 QUIC VarInt 编码，避免把 `0x41` 单字节误当成完整值。

**验证**

49 个 `dhttp::protocol` 测试通过，覆盖 known、reserved、unknown、empty、malformed 和队列边界。

### `94e1cee chore(stream): add end-to-end QUIC bridge diagnostics`

**现象**

HAR 13-19 显示 worker 已完成 WebSocket write/flush，server qlog 也可能显示对应 packet 被 ACK，但浏览器没有 `OnReadComplete`。仅靠 gateway 字节计数无法判断数据停在 worker IPC、根进程 bridge、QUIC writer，还是浏览器 H3 DATA 接收路径。

**根本原因**

既有日志缺少跨层关联字段和开始/完成边界，无法把同一 read/write 从 worker 追踪到根进程 QUIC stream。

**修复方案**

为 connection、IPC bridge、RPC reader/writer 和 hypervisor read/write 增加 stream id、操作序号、字节数、开始/完成/失败日志；扩展双向多帧测试，使日志能揭示卡在某一层的具体操作。

**验证**

IPC 单向 data transfer、unavailable 和 writer EOF 专项测试通过。该提交只增强可观测性，不把 packet ACK 等同于浏览器应用层已交付。

### `7e4bae5 fix(dhttp): keep classifier diagnostics side-effect free`

**现象**

增加分类日志后，`listen_connection_continues_after_stream_id_lookup_failure` 稳定超时：一次本应由监听层观察的 reader stream-id failure 被诊断代码提前消耗。

**根本原因**

诊断逻辑额外调用了 reader 的异步 `stream_id()`。该接口可失败且实现可以带状态，日志读取因此改变了业务状态机。

**修复方案**

从同一双向流的 writer 侧取得仅用于日志的 stream id，不再触碰 reader 的状态；实际分类仍只消费 reader payload。

**验证**

端点测试确认 stream-id lookup 失败后仍能继续处理下一条合法 request stream，同时原“失败后关闭连接”路径也通过。

## 3. dhttp 提交

### `ac620f7 build: use local h3x transport fixes`

**现象**

只修改本地 `h3x` 不会影响从 registry 解析依赖的 `dhttp`，最终 pishoo 可能仍链接发布版 h3x。

**根本原因**

Cargo 的依赖解析默认使用 registry source；三个仓库虽在相邻目录，但不会自动组成同一 source graph。

**修复方案**

在本地 dhttp workspace 中增加 `../h3x` path dependency/patch，使 dhttp 0.6.0-beta.5 确定使用上述本地 h3x 提交。

**验证**

Cargo lock graph 中 h3x 不再带 registry source/checksum，pishoo release 构建使用同一份本地 h3x。

## 4. pishoo 提交

### `848102a feat(pishoo): bind shared network from root config`

**现象**

机器存在多个网卡/虚拟接口时，共享 DHTTP network 和 `sky.lee~` server 可能监听到非预期地址，导致 QUIC 路径和回包接口不稳定。

**根本原因**

server 级 listen 已有接口表达能力，但根级共享 network 创建时没有读取等价配置。

**修复方案**

新增根级 `network_listen` 解析、配置访问器和构建传播，将 `network_listen en0 0` 转换成 `iface://en0:0` bind pattern 并传给共享 `DhttpNetwork`。

**验证**

解析测试确认接口 scope 被保留；运行时主 QUIC UDP 只监听 en0 的当前 IPv4/IPv6 地址，通配 5353 仅用于 mDNS。2026-07-29 最终重启后，en0 IPv4 为 `192.168.5.119`。

### `ba1b09d perf(reverse): cache client identity per connection`

**现象**

同一浏览器连接加载大量 HA 资源时，每个 HTTP 请求都会重新查询 remote authority，增加 IPC/authority 往返和首屏延迟。

**根本原因**

access middleware 没有利用 DHTTP connection identity 在连接生命周期内不变这一事实。

**修复方案**

按 `Arc<ConnectionState>` identity 缓存客户端名称；缓存 key 使用 `Weak`，失效连接自动清理。authority 查询失败仍不缓存允许结果，访问控制继续 fail closed。

**验证**

访问控制测试保留 backend error 返回 403 的语义；实际 access log 可看到同一连接后续静态资源和 `/api/onboarding` 正常通过。

### `70fb6b4 fix(reverse): reuse HTTP upstream connections`

**现象**

HA 页面每个资源请求都新建到 `127.0.0.1:8123` 的 TCP 连接，造成大量短连接；原来强制的 `Connection: close` 也直接禁止复用。代理响应还可能把 hop-by-hop header 错误带到 H3 一侧。

**根本原因**

HTTP upstream 路径手工 `TcpStream::connect` 并为每个请求建立新的 Hyper HTTP/1 connection；请求 header 处理硬编码 `Connection: close`，且没有实现 RFC hop-by-hop header 及 `Connection` 指定扩展字段的清理。

**修复方案**

- 使用进程共享 Hyper client 和连接池。
- idle timeout 90 秒，每主机最多 32 个 idle connection。
- 删除强制 `Connection: close`。
- 请求和响应两侧清理标准 hop-by-hop header，以及 `Connection` 中点名的扩展 header。
- 为 upstream 构造完整 URI，保持既有 proxy redirect、Location 和 Refresh 的外部 origin/path 语义。

**验证**

测试连续发送两个请求并确认 upstream 只接受一次 TCP connection；header 测试确认 `Connection` 和被点名字段均被移除。

### `145ac41 fix(reverse): buffer bounded upstream responses`

**现象**

有限长度的 HA JSON/静态响应偶发在 Hyper HTTP/1 产生的小 body chunk 之间被截断，上层看到不完整页面或 onboarding 请求失败。

**根本原因**

小响应仍以 streaming body 跨 worker IPC 逐块转发；当 worker-side stream 在 chunk 间进入 teardown，后续数据无法交付。

**修复方案**

对带 `Content-Length` 且不超过 2 MiB 的未压缩响应先完整 collect，再作为一个 body 转发。大响应和未知长度响应继续 streaming，避免无界内存使用；gzip 路径保持流式压缩并传播 collect error。

**验证**

`/api/onboarding` 等有限 HTTP 响应能够完整返回；单元测试覆盖 bounded response 路径。

### `816ac05 fix(reverse): flush each WebSocket tunnel chunk`

**现象**

HAR 13 显示 HA 已返回结果，gateway 也完成 write，但后续 WebSocket 下行帧可能停留在缓冲区；HAR 14 又证明 122 字节 `auth_ok` 也会丢，排除了单纯 MTU/大包假设。移除 `permessage-deflate` 会让认证立即退化。

**根本原因**

通用 `copy_bidirectional` 不保证每个小块写入后立即 flush，而 h3x AsyncWrite 会缓冲小 payload。长连接在单帧后可能保持空闲，没有后续写入自然推动 flush。WebSocket extension 若未端到端透传，还会造成两端压缩状态不一致。

**修复方案**

- 将双向复制拆成两个独立 `copy_and_flush` loop。
- 每次 `write_all` 后显式 `flush`，EOF 时 shutdown writer。
- 保留并透传 `Sec-WebSocket-Extensions: permessage-deflate`。
- 增加双向 read/write/flush 字节计数和连续帧/burst 诊断。

**验证**

测试覆盖客户端连续帧、服务端 8 帧 burst，以及“源连接保持打开时，小消息仍必须立即 flush”。强制 512 字节拆包和移除压缩的实验已撤回，不在该提交中。

### `f1b539f feat(pishoo): enable server qlog from environment`

**现象**

仅有浏览器 netlog 和 gateway 日志时，无法确认 server QUIC packet 是否生成、发送和被 ACK。

**根本原因**

pishoo 构建 server QUIC config 时没有暴露 dquic telemetry/qlog logger。

**修复方案**

启用 `qlog` feature 后读取 `PISHOO_QLOG_DIR`；创建目录并用进程级 `OnceLock` 复用 `LegacySeqLogger`，把 logger 注入所有注册 endpoint 的 server config。未设置环境变量时行为不变。

**验证**

运行日志会打印 qlog 目录；server qlog 可与 HAR/netlog 按时间、stream 和 packet ACK 交叉核对。

### `2080ccb build: wire local dhttp and h3x fixes`

**现象**

pishoo 顶层依赖仍可能从 registry 解析 dhttp/h3x，从而绕过相邻仓库中的修复。

**根本原因**

workspace dependency 和传递依赖需要统一覆盖，否则同名包可能来自不同 source。

**修复方案**

把 pishoo 的 dhttp 指向 `../dhttp/dhttp`，h3x 指向 `../h3x`，并用 `[patch.crates-io]` 覆盖传递 h3x；更新 lockfile 使 source graph 唯一。

**验证**

release 构建成功，lockfile 中 dhttp 0.6.0-beta.5 和 h3x 0.6.0-beta.4 均解析为本地 source。

## 5. 提交依赖与应用顺序

建议按下列顺序审查或移植：

1. 在 h3x 基线 `776cbfb` 上依次应用 `645b452`、`a0dc765`、`8a50f79`、`e96a02f`、`78bdfdd`、`94e1cee`、`7e4bae5`。
2. 在 dhttp 中应用 `ac620f7`，使其使用本地 h3x。
3. 在 pishoo 基线 `adbdda3` 上依次应用 `848102a`、`ba1b09d`、`70fb6b4`、`145ac41`、`816ac05`、`f1b539f`、`2080ccb`。

其中 `94e1cee` 和 `f1b539f` 是诊断提交；若产品分支不需要详细日志/qlog，可以独立不移植，但排障能力会下降。`7e4bae5` 依赖 `78bdfdd` 中的 classifier 诊断代码。

## 6. 验证结果与已知限制

已通过：

- h3x `cargo fmt --check`
- h3x 49 个 `dhttp::protocol` 专项测试
- h3x stream-id failure 后继续处理下一请求的端点回归测试
- h3x request receive-side drain 回归测试
- h3x IPC writer EOF 只影响当前 stream 的回归测试
- h3x IPC `open_uni_data_transfer` 和 `open_uni_unavailable` 单独运行通过
- pishoo release 构建
- WebSocket 专项测试
- gateway 常规测试本次为 113 通过，3 个测试因本机缺少 keychain/`test.genmeta.net` 测试证书失败

已知与本修复无关的环境/基线问题：

- h3x 全量测试中的 `dquic::network::tests::canceled_unbind_completes_cleanup_before_rebind` 在基线代码的最终 `rebound.unbind().await` 挂起；该文件相对 `776cbfb` 没有修改。
- 若跳过上述挂起用例，全量并发运行仍有依赖 loopback/network timing 的 dquic 测试失败；对应单向 IPC 专项测试单独运行通过。
- pishoo `cargo test -p gateway --all-features` 在 macOS 会因无关的 PAM package 编译失败，不代表默认 gateway feature 失败。

## 7. 运行时配置（不属于 Git 提交）

`spike.liu` 白名单存储在 `/Users/lixiaofeng/.dhttp/sky.lee/db/access.db`，实际客户端 identity 是 `spike.liu.dhttp.net`。规则使用：

- infix：`spike.liu~`
- polish：`"spike.liu.dhttp.net" `
- action：allow（0）
- location：`sky.lee~` 根 location（id 1）

重启 pishoo 后，access log 已确认该 identity 对首页静态资源、manifest 和 `/api/onboarding` 获得 200。该数据库变更不应误记为上述任一源码提交。

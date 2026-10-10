# 2026 年 10 月 9 日 Pishoo 传输测试报告

本机真实 UDP、QUIC、TLS 和 HTTP/3 验收共完成 65 个场景：59 个通过、6 个失败。1 GiB 静态下载、1 GiB 代理上传并回显、双向请求、1 至 128 路并发、取消隔离和 30 分钟混合负载通过。未达到全通过：大 WASM Echo 会耗尽 fuel，空闲和慢读取会触发 QUIC idle timeout；独立进程首次建连在附加复测中多次失败。

生产类型、字段、接口、配置和依赖版本没有修改。新增内容是显式运行的隔离测试、测试脚本和报告。

后续已完成 Mac 与 Ubuntu 的真实跨设备/NAT 补测，13 个场景全部通过，见[跨设备与 NAT 补测报告](transport-public-2026-10-09.md)。补测使用本地 QUIC OCSP 兼容修改，版本和范围与本报告不同，原失败记录保持不变。

## 测试环境与验证范围

| 项目 | 实测环境 |
| --- | --- |
| 时间 | 2026-10-09 18:35:08 至 19:15:03，Asia/Shanghai |
| 源码基线 | `21b96fdd078b2485fb39ad253228355a4ac242ee`，加本次未提交的测试代码 |
| 主机 | macOS 15.7.3，Darwin 24.6.0，arm64，10 核，16 GiB 内存 |
| 构建 | Rust 1.97.1，locked/offline debug 测试构建，Tokio 4 个 worker |
| 主矩阵 | 同一进程内的 receiver、alice、bob 身份；真实 UDP 和完整 Pishoo Router |
| 独立进程 | 一个 Router 服务端、两个具名客户端进程；同一台 Mac |
| 路径 | 目标地址为 127.0.0.1；Network 也可能使用本机内网源地址，测试监听允许 Loopback 和 Internal |
| 授权与认证 | 临时 CA、证书、私钥、有效签名 OCSP；TLS 名称及凭据验证、daccess 授权保持开启 |

使用 `Server.load` 构建生产 Router，经原生 `Endpoint.listen` 交付服务，覆盖授权层、`/std/file`、WASM Echo、回环 HTTP/1.1 代理及 HTTP/3 Body。每个大文件按块读取并计算 SHA-256，不用 `collect()` 缓存整个文件。下载同时核对 Content-Length、实际字节数和 SHA-256；Echo 核对上传与响应的字节数及 SHA-256。并发 Echo 使用不同种子的数据，检测串流或内容错配。

所有身份、数据库、组件和测试文件均位于临时 home。没有操作日常身份、线上 DNS 或现有服务。自动 Network 仍会扫描系统网卡并维护 STUN；本次目标解析直接指向本机测试 socket。

Cargo 从临时工作目录解析配置，避开本机父目录的 path patches；依赖来自发布清单的固定 Git revision：dhttp `f8bdd270`、dquic `34a713b4`、h3x `c26abcd3`、daccess `3e3ac619`，没有使用相邻源码目录。

## 传输与生命周期结果

主矩阵共 55 个场景，最终独立进程矩阵共 10 个场景。完整逐项结果及 hash 见[机器可读结果](transport-testing-2026-10-09.json)。

| 场景 | 结果 | 实测内容 |
| --- | --- | --- |
| 静态文件边界 | 通过 | 0、1、4095、4096、65535、65536、65537 字节，以及 1 MiB、100 MiB、1 GiB，内容全部一致 |
| 1 GiB 静态下载 | 通过 | 139.79 秒，7.33 MiB/s |
| 1 GiB HEAD | 通过 | Content-Length 正确，响应 Body 为 0 字节 |
| 100 MiB 代理下载 | 通过 | 13.10 秒，7.64 MiB/s |
| 代理上传并回显 | 通过 | 0 至 1 MiB 的边界尺寸、100 MiB、1 GiB 全部一致；1 GiB 用时 102.64 秒 |
| WASM Echo | 部分通过 | 0 至 1 MiB 的边界尺寸通过；100 MiB 流重置，独立诊断确认 fuel 耗尽 |
| 同一 Endpoint 并发 | 通过 | static、wasm、proxy 各测 1、8、32、128 路，每流 1 MiB，共 12 组 |
| A 和 B 同时反向请求 | 通过 | A 请求 B、B 请求 A，各下载 100 MiB；GET 请求方向与文件载荷方向相反 |
| 两个身份同时请求 | 通过 | 同进程两个具名客户端，共 16 路、每流 1 MiB |
| 大小请求混合 | 通过 | 1 GiB 下载期间完成 32 个 1 MiB 请求，后者 p95 0.153 秒、p99 0.170 秒 |
| 不空闲的逐块双工 | 通过 | 两个独立客户端各验证 3 块；每块在下一块输入和上传 EOF 前完整返回 |
| 双工空闲 31 秒 | 失败 | WASM、代理各 1 个失败，第二块发送或读取时观察到 idle timeout |
| 慢写 | 通过 | WASM、代理各上传 2 MiB，每写 64 KiB 暂停 20 ms |
| 每响应 Frame 暂停 20 ms | 失败 | WASM、代理各读取 2 MiB，均触发 idle timeout，未读完 |
| 每响应 Frame 暂停 1 ms | 部分通过 | 独立进程 Alice 完成 2 MiB，Bob 在约 20.54 秒触发 idle timeout；Frame 大小和实际 sleep 粒度会影响有效读取速率 |
| 上传与响应中途取消 | 通过 | WASM、代理分别取消 16 条流，后续下载正常，WASM 任务回收为 0 |
| 大下载提前丢弃 | 通过 | 丢弃 16 个 1 GiB 响应，后续小下载正常 |
| 取消隔离 | 通过 | 取消一个大下载，同时 8 个 4 MiB 代理 Echo 全部完成 |
| 匿名访问 | 通过 | 具名授权的测试文件对匿名请求返回 403 |
| 独立进程同步下载 | 通过 | 两个已建连客户端同步下载各 100 MiB，完整性一致，分别用时 20.54、19.88 秒 |
| 独立进程首请求 | 最终轮通过，复测有失败 | 最终轮两个首 GET 均成功；5 轮共 10 个首 GET 中 4 个建连失败，保留全部历史结果 |
| 30 分钟混合负载 | 通过 | 10,288 次请求，用时 1800.35 秒，所有大小和 hash 一致 |
| 应用关闭 | 通过 | Server.close 完成，剩余 WASM 任务为 0；不把它解释为全局 Network 主动关闭 |

## 并发耗时

每组所有请求都通过完整性检查。并发数指同时启动的请求 future 数量；DQUIC 默认初始双向流额度为 100，超出的请求由底层流控等待，所以 128 个请求全部完成不等于 128 个流始终同时活跃。下表记录 debug 构建在本机的观察值，包含客户端生成数据、SHA-256、两端处理和 WASM 执行；部分阶段同时运行了诊断任务。它不是隔离的 release 性能基准，也不是公网带宽测量。Echo 的合计速率只计算一份 payload，上传和响应 Body 合计约为其两倍，不含 HTTP/3 与 QUIC 开销。

| 并发数 | 路径 | 整组耗时秒 | 单请求 p95 秒 | 合计 MiB/s |
| --- | --- | --- | --- | --- |
| 1 | static | 0.071 | 0.071 | 14.06 |
| 1 | wasm | 0.114 | 0.114 | 8.76 |
| 1 | proxy | 0.104 | 0.104 | 9.63 |
| 8 | static | 0.418 | 0.417 | 19.16 |
| 8 | wasm | 0.854 | 0.842 | 9.37 |
| 8 | proxy | 0.818 | 0.706 | 9.78 |
| 32 | static | 2.042 | 2.040 | 15.67 |
| 32 | wasm | 3.535 | 3.353 | 9.05 |
| 32 | proxy | 3.311 | 3.174 | 9.67 |
| 128 | static | 7.001 | 6.984 | 18.28 |
| 128 | wasm | 13.989 | 12.824 | 9.15 |
| 128 | proxy | 13.734 | 12.524 | 9.32 |

## 首次建连与长流问题

### 独立进程首次建连失败

5 轮双客户端启动共有 10 个首 GET，4 个失败，错误均为 `QUIC: No viable network path exists ... Can't assign requested address (os error 49)`。失败发生在连接获取阶段，尚未获得 HTTP 响应；失败的客户端随后发起 8 路 WASM 请求均能成功。最终轮两端首 GET 均成功，不能覆盖掉前四轮的失败。

问题在当前多网卡 Mac 上可重复出现。日志显示成功连接使用本机回环或内网源地址，具体哪条候选路径导致 error 49 尚未定位。需要检查 DQUIC 的候选路径、socket 可用性和首次建连时序；目前不能宣称独立进程冷启动可靠性通过。原始证据在 `target/transport-process-*20261009/`。

### 空闲和慢读取触发连接超时

WASM 和代理双工完全空闲 31 秒后均失败。2 MiB 响应按 Frame 暂停 20 ms 的两个用例也失败；最终独立进程的 1 ms 慢读用例一端成功、一端失败，错误包含 `ClosedCriticalStream` 和底层 `connection idle timeout`。应用仍在逐帧读取并不保证 QUIC 链路持续产生网络活动。

固定 DQUIC revision 的[默认参数](https://github.com/genmeta/dquic/blob/34a713b48535892ad9843cf2bb0db285e0d09439/qbase/src/param/core.rs)中，客户端 MaxIdleTimeout 为 20 秒，服务端为 30 秒，协商后取较小的 20 秒。因此，应用没有固定 30 秒总执行期限，并不意味着空闲传输会永久存活。长会话需要明确保活和超时策略；慢读取场景还需核对底层缓冲数据在超时后的处理。本次没有修改参数、增加保活状态或自动重试来掩盖失败。

### 大 WASM Echo 耗尽 fuel

100 MiB WASM Echo 返回流重置：`NoError (0x100) ... app error code: 256, final size: 91759845`。final size 含流上的 HTTP/3 帧，不能直接当作已收到的文件字节数。

使用同一 Echo 组件和生产相同的 `100_000_000` fuel，直接运行 guest 的诊断收到 90,071,040 字节后得到 `wasm trap: all fuel consumed by WebAssembly`，剩余 fuel 为 0。诊断作为预期 OutOfFuel 的回归通过。结合这个结果，大 Echo 失败属于已存在的执行预算边界；具体可处理字节数受组件和分块方式影响，不是固定的文件大小上限。静态与本机代理的 1 GiB 传输都通过，不能把此现象归为统一的大文件传输上限。

生产[Invocation.execute](../src/sandbox/runtime.rs)在响应 outparam 交付后放开 guest JoinHandle，调用方主要观察 Body 或流错误；网络日志没有提供本次早响应后的 guest trap。因此，HTTP 200 和 `NoError` 标签都不足以判断完整传输成功。建议在现有 guest 任务结束处记录失败原因，并继续由读取结果和完整性检查确认传输，没有必要增加新的 Body 或完成通知接口。

## 资源和持续负载

每秒采样整个主测试进程 RSS 和 CPU，每约 10 秒采样数字文件描述符。这个进程同时包含服务端、客户端、三个身份和 WASM 执行，不能视为单个生产服务端的内存。

| 指标 | 实测 |
| --- | --- |
| 全程采样点 | 2,359 个 |
| 全程 RSS 峰值 | 2.84 GiB，位于 `concurrent_proxy_128` 阶段 |
| 128 路 static 阶段 RSS 峰值 | 1.21 GiB |
| 128 路 WASM 阶段 RSS 峰值 | 2.71 GiB |
| 128 路 proxy 阶段 RSS 峰值 | 2.84 GiB |
| 30 分钟内请求数 | 10,288；固定 8 路混合请求，每批完成后等待 1 秒 |
| 持续负载 RSS 范围 | 97.1 至 567.8 MiB |
| 首 5 分钟 RSS 中位数 | 505.3 MiB |
| 末 5 分钟 RSS 中位数 | 361.6 MiB |
| 持续负载 FD 范围 | 39 至 45 |
| 持续负载最大单请求耗时 | 1.183 秒 |
| 应用关闭后 WASM 任务 | 0 |

并发阶段连续运行，峰值包含前序阶段保留的资源，不能全部归因于当前路径。持续负载的每批包括 3 个 1 MiB 静态下载、3 个 256 KiB WASM Echo 和 2 个 1 MiB 代理 Echo；检查全部响应字节和 hash。资源范围反映本次窗口，长期运行及独立服务端容量应另作隔离测量。

## 回归检查与复现

常规 workspace 回归 132 项通过，15 项按显式条件跳过；其中本次新增的网络和 fuel 验收已单独执行。`cargo check --workspace --all-targets`、修改测试文件的 rustfmt 检查和 `git diff --check` 通过。已有真实 QUIC 早响应用例、双向 Workspace/Chat 用例、真实 H3 WebSocket 用例，以及本次四线程连接探针和 fuel 诊断，分别在独立进程运行通过。

从仓库根目录运行：

```sh
python3 pishoo/tests/run-transport.py --soak-seconds 1800
python3 pishoo/tests/run-transport.py --case process
```

脚本要求正常构建工具、缓存的锁定 Cargo 依赖、Python 3、OpenSSL 3、本机 TCP/UDP 权限和约 1.3 GiB 测试文件空间；用 `DHTTP_TEST_OPENSSL` 指定 OpenSSL 3。脚本自动从临时目录调用 Cargo，以避免父目录配置改变依赖来源；每次创建独立证据目录。默认矩阵和 process 矩阵都保留所有失败项，并在存在失败时退出非零。

| 内容 | 位置 |
| --- | --- |
| 一键驱动 | [run-transport.py](../tests/run-transport.py) |
| 网络验收实现 | [transport.rs](../tests/unit/server/transport.rs) |
| guest fuel 诊断 | [streaming.rs](../tests/unit/sandbox/execution/streaming.rs) 中 `large_echo_reports_fuel_exhaustion` |
| 汇总与逐项 hash | [transport-testing-2026-10-09.json](transport-testing-2026-10-09.json) |
| 主矩阵原始证据 | 本机 `target/transport-acceptance-20261009/` 中 metadata、results、resources 和 test.log |
| 最终独立进程证据 | 本机 `target/transport-process-complete-20261009/`，包含父进程及两个客户端、服务端日志 |
| 常规回归与编译 | 本机 `target/transport-baseline-final.log`、`target/transport-check-final.log` |
| guest trap | 本机 `target/transport-fuel-diagnostic.log` |

原始日志和临时凭据不纳入 Git；JSON 汇总保留可长期引用的测试数据。原始日志中的网卡、STUN 和进程结束时的 QUIC 控制流诊断未被单独计作传输失败，失败判定依赖请求及 Body 结果、字节数、hash 和任务回收。

## 覆盖边界与后续重点

本报告记录本机真实协议测试，包括独立进程。后续补测覆盖了 Mac 与 Ubuntu、双方 RestrictedPort 的真实公网传输；具体结果及直连路径证据见上述补测报告。对称 NAT、公网 DDNS 身份发布、断网或网卡切换、可控丢包与延迟、10 GiB 文件、Range/断点续传、浏览器兼容和 release 容量仍未验收。

后续先定位首次建连的 error 49，明确长空闲及慢读取的保活/超时行为，再根据 Lib 业务处理成本确认大文件执行方式。保留现有 fuel 预算和架构边界；此次没有增加传输配额、执行准入、状态容器或冻结接口成员。

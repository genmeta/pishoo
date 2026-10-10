# 2026 年 10 月 9 日 Pishoo 跨设备与 NAT 补测报告

Mac 与用户提供的 Ubuntu 主机之间，13 个真实 QUIC/H3 传输场景全部通过。双方原生探测均为 RestrictedPort，没有放行新 UDP 端口或修改防火墙。经 STUN 中介引导完成具名 TLS 握手后，日志出现已验证直连路径；1 GiB 下载、正反向 100 MiB、WASM/代理双工与并发、100 MiB 上传回显均核对了完整性。

本报告补充[本机传输报告](transport-testing-2026-10-09.md)。两个报告使用不同 QUIC 源码状态，不能把本次通过结果视为此前空闲超时、fuel 和冷启动失败均已修复。

## 版本与隔离

| 项目 | 实测环境 |
| --- | --- |
| 传输时段 | 2026 年 10 月 9 日约 20:37 至 20:50，Asia/Shanghai；Receiver 正常收尾约 20:52 |
| Mac | arm64，debug 构建，4 个 Tokio worker；本机临时 alice 身份 |
| Ubuntu | 119.28.45.191，x86_64，2 核、约 3.4 GiB 内存，Rust 1.97.1；debug 构建关闭 debuginfo，4 个 Tokio worker |
| Pishoo 与依赖 | 当前 Pishoo 源码；dhttp、h3x、ddns、daccess 沿用锁定源码，QUIC 使用本地 `4e8ba579` 加未提交 OCSP 兼容修改 |
| OCSP 策略 | 允许对端缺少 staple；已提供的 OCSP、证书链、名称、有效期和握手签名仍校验；本次两端临时身份都持有有效 OCSP |
| home 和数据 | 独立临时 CA、alice/receiver 证书、数据库、Echo Lib、生成文件；未修改远端日常身份与既有服务 |
| 端点信令 | 通过 SSH 交换 AddressBook 导出的公网中介地址；未向线上 DDNS 发布测试身份 |

远端在 `/home/ubuntu/pishoo-transport-20261009/` 构建，使用上传的当前源码和 519 个锁定依赖包。临时清单把这些依赖指向对应源码快照，避免额外 Git 检出；不修改正式清单或本机 Cargo.lock。编译单任务并降低调度优先级，远端既有 Pishoo 始终保持 active。

## 路径与端口限制

| 节点 | 内部绑定 | 公开映射 | 探测 |
| --- | --- | --- | --- |
| Mac en0 | 192.168.5.179:58062 | 183.23.105.232:5034 | RestrictedPort |
| Mac TUN | 198.18.0.1:61587 | 183.23.105.232:5035 | RestrictedPort |
| Ubuntu eth0 | 10.0.8.16:55399 | 119.28.45.191:55399 | RestrictedPort |

两端都获得 `35.75.234.52:20002`、`35.78.0.4:20002`、`57.180.82.220:20002` 三个中介地址，并由现有 Network 持续进行 STUN 维护。测试将这些 Mediate 端点注册为解析结果，流量走原生 QUIC，没有 SSH/TCP 隧道承载文件数据。

初始具名握手验证路径为 Mac 中介 `57.180.82.220:20002` 与 Ubuntu 中介 `35.78.0.4:20002`。后续客户端验证路径出现 `198.18.0.1:61587 → 119.28.45.191:55399`；服务端反向请求时出现 `10.0.8.16:55399 → 183.23.105.232:5035`。反向 GET 使用已建立的具名连接池，并在正向 1 GiB 下载期间完成。

**这些日志证明中介引导成功且存在已验证直连路径；没有每路径字节计数，因此不能断言所有文件字节都由直连承担。** 中介路径同时保留，Mac 的验证直连源地址还涉及既有 TUN 网络环境，不能把本次当作完全绕过代理环境的独立 en0 性能基准。

此前普通 UDP 标记探针没有在远端抓到入站，固定 `119.28.45.191:443` 的原生 QUIC GET 也因 `Too many PTOs: 7` 在握手前失败。这些结果只说明被测试的直连方式没有成功，不能据此要求先放行端口或判定中介引导不可用。DQUIC STUN 使用项目编码，普通 RFC STUN 空请求的超时不用于判定原生网络失败。

腾讯云[安全组说明](https://www.tencentcloud.com/document/product/213/12452)确认安全组为有状态过滤，可允许已放行出站流量的反向流量；[RFC 4787](https://www.rfc-editor.org/rfc/rfc4787.html)将地址及端口依赖过滤定义为另一项 NAT 行为。实际探测观察的是整条路径的合并效果。本次保留原有规则，由原生探测实测为 RestrictedPort，再成功建连；这一结果支持“在当前未新增 UDP 入站规则的环境仍可经中介引导通信”，不代表任意安全组策略都能穿透。

## 逐项结果

所有响应确认 HTTP/3 和已验证远端身份。下载逐块计算 SHA-256，并核对 Content-Length 与字节数；Echo 上传与读取响应并行推进，核对两端 hash。并发是同时启动的请求数量，WASM 与代理分别测试 1、8、32 个请求，每请求 1 MiB。

| 场景 | 结果 | 耗时秒 | 完整性证据 |
| --- | --- | --- | --- |
| 远端至本机 1 MiB | 通过 | 1.682 | 1,048,576 字节，SHA-256 一致 |
| 远端至本机 100 MiB | 通过 | 51.713 | 104,857,600 字节，SHA-256 一致 |
| 远端至本机 1 GiB | 通过 | 553.744 | 1,073,741,824 字节，SHA-256 一致 |
| WASM 逐块双工 | 通过 | 1.253 | 3 块均在上传 EOF 前完整返回 |
| WASM 并发 1 | 通过 | 0.882 | 1 个请求，各 1 MiB，内容一致 |
| WASM 并发 8 | 通过 | 4.795 | 8 个请求，各 1 MiB，内容一致 |
| WASM 并发 32 | 通过 | 22.761 | 32 个请求，各 1 MiB，内容一致 |
| 代理逐块双工 | 通过 | 2.091 | 3 块均在上传 EOF 前完整返回 |
| 代理并发 1 | 通过 | 1.381 | 1 个请求，各 1 MiB，内容一致 |
| 代理并发 8 | 通过 | 5.388 | 8 个请求，各 1 MiB，内容一致 |
| 代理并发 32 | 通过 | 22.429 | 32 个请求，各 1 MiB，内容一致 |
| 100 MiB 代理上传并回显 | 通过 | 65.034 | 104,857,600 字节，SHA-256 一致 |
| 本机至远端反向下载 100 MiB | 通过 | 29.716 | 104,857,600 字节，SHA-256 一致 |

1 GiB 下载用时 553.744 秒，payload 速率约 1.85 MiB/s；反向 100 MiB 用时 29.716 秒，约 3.37 MiB/s；100 MiB 代理上传回显用时 65.034 秒，按一份 payload 计约 1.54 MiB/s。Echo 的上传加响应 Body 流量约为 payload 两倍，不含 QUIC 开销。网络、硬件、构建参数与后台负载不同，这些值不能直接与本机报告的数字作性能优劣比较。

1 GiB 文件 SHA-256 为 `c0c64fff20a8c1f2fa5dbd29305695a54d3e4616ebfa524a1ef71f1a1137317f`；正反向 100 MiB 文件为 `6ba2e0e5770f4d1e61619da7479a36c272aa04e7a85125ea69e92745850bf069`。完整结果、每个场景的 hash、端点与路径日志摘录见[JSON 数据](transport-public-2026-10-09.json)。

## 回归与收尾

当前本地 QUIC 构建的常规 workspace 回归 132 项通过、17 项按显式条件跳过。OCSP 相关 10 项单元测试及旧客户端缺失 OCSP 握手回归均通过；新增测试文件格式和 diff 检查通过。

Alice 测试进程 55001 和远端 Receiver 2301539 均正常退出，应用执行资源由原有 Server.close 回收。远端临时 UDP 探针已按自身期限退出；安全组、防火墙和原有远端 Pishoo 未改动。测试源码、构建缓存和日志保留在独立目录，便于复现；临时 TLS 私钥完成测试后移除，不进入仓库。

本机供用户浏览器测试的新 Pishoo 保持运行，PID 51126，二进制为 `target/debug/pishoo`，使用 `/Users/lixiaofeng/.dhttp`。启动及请求日志为 `target/local-pishoo-20261009.log`，运行诊断仍追加至该 home 的 `logs/error.log`。这些日志属于日常实例，不与临时 NAT 测试数据混合。

| 证据 | 位置 |
| --- | --- |
| 双端 NAT 与握手日志 | 本机 `target/transport-public-evidence/public-alice.log`、`public-receiver.log` |
| 双端场景结果 | 同目录 `public-alice-results.jsonl`、`public-receiver-results.jsonl` |
| 实际端点及文件 hash | 同目录 `alice-ready.json`、`receiver-ready.json` |
| 远端构建记录 | `/home/ubuntu/pishoo-transport-20261009/build.log`、`build.jsonl` |
| 隔离远端原始日志 | `/home/ubuntu/pishoo-transport-20261009/public-receiver.log` |
| 验收代码 | [transport.rs](../tests/unit/server/transport.rs) 的 `transport_public_peer` |
| 本机新版构建来源 | 本机 `target/local-quic-metadata.json`、`local-quic-build.log`、`local-pishoo-start.json` |

## 覆盖边界

此次覆盖 Mac 与这台 Ubuntu 的当前网络、两个 RestrictedPort 路径以及独立进程，未覆盖对称 NAT、不同安全组策略、IPv6 公网、强制丢包或延迟、断网切换、10 GiB、release 性能及线上 DDNS 身份发布。没有重跑 30 分钟公网持续负载；本机 30 分钟记录保留在原报告。本次连接始终有活动数据，不能据此推翻原报告中空闲 31 秒或慢读的失败；100 MiB 走本机 TCP 代理，不用于声称 WASM fuel 限制已消失。

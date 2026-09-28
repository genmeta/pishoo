# 单命令 exec 第一版接口

本清单遵循[设计准则和接口约束](README.md)。第一版只在当前 Pishoo 服务账号下执行一次宿主程序；不提供交互 shell、PTY、WASM shell、子进程 IPC、文件投影或 OS 沙箱。调用者必须是与 Server 同名的已验证 DHTTP 身份。子进程可以访问该服务账号原本有权访问的文件和网络；固定工作目录不构成文件隔离。

## 配置与归属

每个 Server 的 `db/config.db` schema v1 `settings(listen,exec)` 恰好一行；`exec=0/1` 是本版 exec 入口的关闭/启用开关。不增加实例配置或迁移版本。开关改变需重启。

`Server` 直接持有 exec 任务跟踪器，不增加 ExecManager、会话结构或状态包装：

```rust
struct Server {
    profile: dhttp_home::identity::IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    access: std::sync::Arc<access_control::AccessService>,
    router: std::sync::Arc<std::sync::RwLock<axum::Router>>,
    sandbox: Sandbox,
    exec_tasks: tokio_util::task::TaskTracker,
}
```

`exec_tasks` 跟踪实际持有 `tokio::process::Child` 的本方任务，不限制同时执行的 exec 数量。关闭 Server 时关闭任务登记，再等待已登记任务；超时报告 `ShutdownDeadline`，不谎称进程已回收。

唯一新增的跨模块接缝为：

```rust
pub(crate) async fn execute(
    enabled: bool,
    server_name: &str,
    cwd: &std::path::Path,
    tasks: tokio_util::task::TaskTracker,
    request: http::Request<dhttp::Body>,
) -> Result<http::Response<dhttp::Body>>;
```

其他解析、有限读取和进程回收只使用 exec 模块内部的无状态辅助函数及调用栈局部变量，不增加持久结构。h3x 和 dhttp 接口不变。

## HTTP 协议

精确路径为 `POST /exec`，`Content-Type: application/json`。Server 加载和重载时将它与 Lib 路由合并，统一经过 daccess 授权，再检查同名身份；禁用时对已获 daccess 准入的请求返回 404，非 POST 返回 405。请求是 JSON object：

```json
{"program":"git","args":["status"],"stdin_base64":""}
```

`program` 必需，`args` 默认空数组，`stdin_base64` 默认空字节；拒绝未知字段、非字符串参数、NUL、请求 trailers 和超限数据。整个 JSON 不超过 2 MiB，解码后 stdin 不超过 1 MiB；至多 128 个参数、合计至多 64 KiB。程序名直接交给 `Command::new`，参数逐项交给 `arg`；Pishoo 不解析 shell 字符串。调用者仍可显式指定一个 shell 程序。工作目录固定为本 Server 身份目录；子进程环境清空，仅设置固定 PATH、HOME 和 LANG。启动前拒绝以 root 身份运行的 Pishoo 进程。

成功启动并执行完后返回 HTTP 200，JSON 包含 `exit_code`、`signal`、`stdout_base64`、`stderr_base64`。非零退出码仍是执行结果，不改为 HTTP 错误。stdout 和 stderr 各自最多 1 MiB，超过则终止命令并报错。输入无效返回 400，同名身份不符或匿名返回 403，关闭后拒绝返回 503，超时返回 504。

## 生命周期和能力边界

请求完整且有界地读完 JSON 后检查任务登记是否关闭，再启动子进程。每个子进程只由一个受跟踪任务持有；请求 future 持有现成 `DropGuard`，提前丢弃时取消该任务。任务同时驱动 stdin、stdout、stderr 和 Child.wait，最长执行 30 秒。单次请求取消、超时或输出超限时对本次进程组发 TERM；2 秒未退出则发 KILL 并等待直接 child 回收。Server 不批量取消已启动的 exec；关闭时等待已登记任务，受 15 秒退出期限约束。

此模式采用用户明确选择的 Pishoo 服务账号权限，不提供文件/网络隔离。进程组可覆盖普通后代，但程序主动 `setsid` 或 double-fork 后不保证后代仍属于该组；若将来需要任意恶意程序的整树回收和硬资源配额，须单独引入 OS 隔离与回收能力，不能声称本版已具备。

2026-09-27 用户将冻结终端设计收缩为上述宿主单命令 exec：移除原 `TerminalManager`、会话/帧/PTY、WASM shell、文件 broker、平台 helper 与相关 WIT；保留每 Server 的 `settings.exec` 和同名身份准入。

2026-09-28 用户要求移除 exec 并发名额与 Server 身份取消信号：删除 `exec_slots`、`Server.cancel` 及 `execute` 的相应参数；保留每次请求的取消与子进程回收。

# SSH Session 生命周期收束设计

状态：**Proposed，等待接口评审，尚未修改实现。**

## 1. 目标和验收标准

修复 `genmeta ssh` 客户端退出后 `pishoo-ssh-session` 与其交互 shell 长时间不释放的问题。修复后需要同时满足：

1. 正常执行 `exit` 后，helper 和 shell 都退出。
2. 客户端被强制结束、网络路径消失或 WebTransport 被远端关闭后，helper 和 shell 都在 5 秒内退出。
3. worker/root 关闭 session helper 时，先触发受控清理，超时后才强杀。
4. 任意结束路径都回收直接子进程，不留下 zombie；普通交互 shell 不成为 PPID 1 的孤儿。
5. 连续建立并异常断开 20 次，`pishoo-ssh-session` 和其 shell 数量回到测试前基线。

非目标：清理由用户命令主动 `setsid()` 后完全脱离 SSH session 的守护进程。与 OpenSSH 一样，这类显式 daemonize 进程不应由连接生命周期无限追踪。

## 2. 当前实现与根因

现场中没有存活的 `genmeta ssh` 客户端，但存在 12 个运行约 90-112 分钟的 `pishoo-ssh-session`。多数 helper 仍有 `-zsh` PTY 子进程；旧 helper 被重启杀死后，还有若干 `-zsh` 变成 PPID 1。这不是活动监视器显示延迟，而是实际生命周期泄漏。

根因由两个缺口组成：

1. `gateway/src/reverse/sshd.rs::run_ssh_session` 只等待 worker token、remoc connection 和 `StartSessionFn` 返回，没有等待真实 `WebTransportSession::closed()`。客户端物理连接已经关闭时，这三个 future 仍可能全部 pending，因此后面的 `session_shutdown.cancel()` 和 remoc drop 永远不会开始。
2. `dssh::session::process` 持有原始 `tokio::process::Child`，但没有外部 cancellation 分支。上层 future 被丢弃时，Tokio 默认不会杀死子进程；即使启用 `kill_on_drop`，它也只保证直接 child，不表达当前 PTY process group 的所有权和分级终止策略。

`pishoo-ssh-session` 还放大了第二个问题：session token 在 `run_session()` 返回后才取消，而 remoc connection 结束时主函数直接退出，没有先取消并等待正在运行的 session task。

## 3. 生命周期不变量

实现必须维持以下不变量：

- 一个已接受的真实 WebTransport session 只有一个 gateway supervisor。
- supervisor 收到任一终止事件后只进入一次 shutdown，shutdown 是单向且幂等的。
- helper 主任务退出前，必须取消并等待所有 session task。
- 每个 shell/command 只有一个 `SessionProcess` owner；owner 正常完成前必须 `wait()`，异常 drop 时必须至少 kill process group。
- channel writer 已不可达时，清理不得等待向远端发送 EOF/Close；本地进程回收优先。
- root reaper 是最后兜底，不承担识别 shell PID/PGID 的职责。

## 4. 方案对比

| 方案 | 改动 | 能否修复 helper | 能否修复孤儿 shell | 结论 |
| --- | --- | --- | --- | --- |
| A. 只在 gateway 等待 `session.closed()` | 小 | 是 | 不保证，RPC future drop 仍可能丢弃 `Child` | 不完整 |
| B. 增加 20 秒 TTL/定时扫描 helper | 小到中 | 延迟修复 | root 不知道 shell PGID，可能继续孤儿 | 掩盖根因 |
| C. 连接信号、helper task scope、进程 owner 端到端收束 | 中 | 是 | 是，正常 shell/process group 可确定回收 | **推荐** |

推荐 C。它不修改协议，也不需要在 `h3x` 新增抽象；只把已有的真实关闭信号沿现有层级传递到实际资源 owner。

## 5. 外部实现交叉验证

OpenSSH server loop 直接监视客户端连接；连接 read 失败或 keepalive 超时会退出 server loop，随后执行 channel 和 session 全量销毁，而不是等待登录 shell 自发退出。它也通过 SIGCHLD/`waitpid` 关闭对应 session。这支持“transport close 是 supervisor 的一等终止事件，进程 owner 负责 reap”的设计：

- [OpenSSH serverloop.c](https://github.com/openssh/openssh-portable/blob/master/serverloop.c)
- [OpenSSH session.c](https://github.com/openssh/openssh-portable/blob/master/session.c)
- [OpenSSH channels.c](https://github.com/openssh/openssh-portable/blob/master/channels.c)

Tokio 的 `Command::kill_on_drop` 文档说明默认 drop 不会杀 child，而且 Unix child 仍需 reap。因此它只能作为 backstop 的一部分，不能替代显式 shutdown 和 wait：

- [Tokio Command](https://docs.rs/tokio/latest/tokio/process/struct.Command.html)

## 6. 推荐架构

![SSH session 生命周期](/Users/lixiaofeng/code/genmeta-rc/pishoo/design/ssh-session-lifecycle.png)

### 6.1 Gateway：真实 transport 是起点

文件：`gateway/src/reverse/sshd.rs`

保留现有 `run_ssh_session` 入口，增加一个仅在模块内可见的结束原因枚举：

```rust
#[derive(Debug)]
enum SshSessionEnd {
    SessionFinished,
    TransportClosed(dhttp::h3x::webtransport::CloseReason),
    WorkerShutdown,
    RemocClosed,
}
```

在构造 IPC adapter 前保留真实 `Arc<WebTransportSession>`，将 `session.closed()` 加入最终 `tokio::select!`。select 只负责确定第一个结束原因；所有分支之后必须统一执行清理：

```text
end = select {
    worker token cancelled       => WorkerShutdown
    real WebTransport closed     => TransportClosed(reason)
    remoc connection completed   => RemocClosed/error
    StartSessionFn completed     => SessionFinished/error
}

session_shutdown.cancel()
drop remoc sender/receiver/connection/session RPC future
wait IPC WebTransport server task
log end reason and return
```

关键点：`TransportClosed` 是正常的 session 终止原因，不应默认记录成 gateway error；协议错误仍随 `CloseReason` 记录供诊断。

### 6.2 Helper：连接和 session task 必须结构化收束

文件：`pishoo/src/bin/pishoo_ssh_session.rs`

在 helper main 中创建一个 `helper_shutdown: CancellationToken` 和 `TaskTracker`。`StartSessionFn` 不再直接内联执行整个 session，而是把 `run_authenticated_session(...)` spawn 到 tracker，并 await 它的 `JoinHandle`。

建议提取：

```rust
async fn run_authenticated_session(
    bootstrap: SessionBootstrap,
    user_info: UserInfo,
    fd_transfer: FdTransfer,
    shutdown: CancellationToken,
) -> Result<(), SessionRunError>;
```

`FdTransfer` 以代码中的实际公开类型替换；这里展示的是接口形状。

helper main 同时等待 remoc connection 与 SIGTERM/SIGINT。任一结束后执行：

```text
helper_shutdown.cancel()
session_tasks.close()
session_tasks.wait()
return
```

spawn 的必要性不是为了并行，而是为了避免 remoc 取消 RPC future 时直接 drop 正在管理 shell 的 future。即使调用方不再 await `StartSessionFn`，tracker 中的 session task 仍会收到 token 并完成本地清理。

### 6.3 Dispatcher：将 shutdown 传给资源 owner

文件：`dssh/src/session/dispatcher.rs`

建议把 API 改为：

```rust
pub async fn run_session<S>(
    conversation: Arc<Conversation<S>>,
    config: SessionConfig,
    shutdown: CancellationToken,
) -> Result<RunSessionOutcome, RunSessionError>;
```

并为结果增加：

```rust
pub enum RunSessionOutcome {
    SessionFinished,
    ConversationClosed,
    Shutdown,
}
```

主 select 增加 `shutdown.cancelled()`。每个 session channel 使用 `shutdown.child_token()`，传入 `run_pty`/`run_piped`。退出 accept loop 后先 cancel channel token，再等待 `channel_tasks`；否则当前“先等待 channel task，再取消其他任务”的顺序仍可能挂住。

转发 channel 也应使用同一个 shutdown 派生 token，保持 SSH connection 的所有 channel 生命周期一致。

### 6.4 Process：显式拥有 process group

文件：`dssh/src/session/process.rs`

建议调整内部调用接口：

```rust
pub async fn run_pty<R, W>(
    channel: SshChannel<R, W>,
    mode: CommandMode<'_>,
    pty: PtyPair,
    config: &SessionConfig,
    term: Option<&str>,
    client_env: &[(String, String)],
    shutdown: CancellationToken,
) -> Result<(), ProcessError>;

pub async fn run_piped<R, W>(
    channel: SshChannel<R, W>,
    mode: CommandMode<'_>,
    config: &SessionConfig,
    term: Option<&str>,
    client_env: &[(String, String)],
    shutdown: CancellationToken,
) -> Result<(), ProcessError>;
```

新增一个私有、单一职责的 owner：

```rust
struct SessionProcess {
    child: tokio::process::Child,
    process_group: nix::unistd::Pid,
    reaped: bool,
}

impl SessionProcess {
    fn new(child: tokio::process::Child) -> Result<Self, ProcessError>;
    fn child_mut(&mut self) -> &mut tokio::process::Child;
    async fn terminate(&mut self) -> Result<std::process::ExitStatus, ProcessError>;
    async fn wait(&mut self) -> Result<std::process::ExitStatus, ProcessError>;
}
```

`terminate()` 复用现有 `session::signal::deliver(pid, signal)`，因为它已优先 `killpg`：

1. 停止 input/output relay 并关闭 PTY master/pipe。
2. process group 发送 SIGHUP，等待最多 1 秒。
3. 未退出则发送 SIGTERM，再等待最多 1 秒。
4. 仍未退出则发送 SIGKILL，并 `wait()` reap。

`Drop` 只作为异常路径 backstop：若 `reaped == false`，同步向 process group 发送 SIGKILL。正常路径和 cancellation 路径必须显式 `wait()`；不能把 Drop 当成主要回收机制。

`run_pty`/`run_piped` 的核心形状：

```text
process = SessionProcess(child)

result = select {
    normal relay and process exit => send exit status, EOF, Close
    shutdown cancelled =>
        stop relay
        close local PTY/pipes
        process.terminate().await
        return Ok(()) without writing to dead remote channel
}

process.wait().await if not already reaped
```

### 6.5 Root reaper：保持兜底职责

文件：`pishoo/pishoo/src/hypervisor/ipc_server.rs`

root 仍只向 helper PID 发信号，不扫描 descendant。helper 收到 SIGTERM 后会通过 `helper_shutdown` 回收自己的 process group。

建议把 helper 收到 SIGTERM 后的等待窗口调整为 4 秒，略大于 process owner 的 HUP + TERM grace 总和；超时后 root 再 SIGKILL helper。若 helper 被直接 SIGKILL，`SessionProcess::Drop` 不会执行，因此 root SIGKILL 只能是最终故障兜底，不能成为常规关闭路径。

## 7. 调用示例

helper 调用 dispatcher：

```rust
let outcome = run_session(
    conversation,
    SessionConfig {
        user: user_info,
        ..Default::default()
    },
    shutdown.child_token(),
)
.await?;
```

dispatcher 启动 PTY channel：

```rust
let channel_shutdown = shutdown.child_token();
channel_tasks.spawn(async move {
    run_pty(
        channel,
        mode,
        pty,
        &config,
        term,
        &setup.client_env,
        channel_shutdown,
    )
    .await
});
```

gateway 观察真实关闭：

```rust
let transport_session = Arc::new(session);
let transport_closed = transport_session.closed();
tokio::pin!(transport_closed);

let end = tokio::select! {
    reason = &mut transport_closed => SshSessionEnd::TransportClosed(reason),
    () = token.cancelled() => SshSessionEnd::WorkerShutdown,
    result = &mut conn => map_remoc_end(result)?,
    result = &mut session_call => map_session_end(result)?,
};
```

## 8. 失败场景矩阵

| 触发事件 | 首个 observer | 清理传播 | 最终 owner |
| --- | --- | --- | --- |
| 用户执行 `exit` | `run_pty`/`run_piped` | dispatcher -> helper -> gateway | `SessionProcess::wait` |
| 客户端进程被 kill | `WebTransportSession::closed` | gateway -> remoc EOF -> helper token | `SessionProcess::terminate` |
| QUIC path timeout | `WebTransportSession::closed` | 同上 | `SessionProcess::terminate` |
| helper remoc 异常 | gateway/helper main | helper token | `SessionProcess::terminate` |
| worker shutdown | gateway token + root reaper | gateway/helper token | `SessionProcess::terminate` |
| shell 忽略 HUP/TERM | process owner timeout | SIGKILL process group | `SessionProcess::wait` |

## 9. 测试设计

### 单元测试

- gateway：transport close future 先完成时，选择 `TransportClosed`，并验证 IPC shutdown token 被取消。
- dispatcher：shutdown 先完成时，不再等待仍 pending 的 accept future，结果为 `RunSessionOutcome::Shutdown`。
- process：启动忽略 HUP/TERM 的进程组，取消 token 后必须进入 SIGKILL 并在 deadline 内 reap。
- process：正常命令退出仍发送 exit-status、EOF 和 Close，避免改变现有协议语义。
- process：主动 drop `SessionProcess` 时 process group 收到 backstop SIGKILL。

Unix signal/process-group 测试使用独立临时进程组，不依赖机器上现有 shell PID。

### 集成测试

1. 连接交互 shell，记录远端 `pid/tty/pgid`，执行 `exit`，断言 helper 和 shell 消失。
2. 连接后直接 SIGKILL 客户端，断言 5 秒内 helper、shell 和 PGID 消失。
3. 模拟 QUIC path idle/transport close，验证与客户端 kill 同一路径。
4. session 活跃时重启 worker，验证 root reaper 与 helper graceful shutdown 配合。
5. 循环 20 次异常断开，进程数量回到基线，且日志每个 conversation 只有一个结束原因。

## 10. 实施顺序

遵循 coding playbook，评审通过后分小步实现：

1. 先加入上述函数签名、枚举和 `todo!()`/最小桩，确保 `pishoo`、`dssh` 编译通过。
2. 实现 gateway transport-close observer 和统一 cleanup，补 gateway 测试。
3. 实现 helper token + tracked session task，补 remoc 取消测试。
4. 实现 dispatcher cancellation 和 `SessionProcess`，补进程组测试。
5. 调整 root reaper grace，执行端到端反复连接/断开验证。

## 11. 待评审决策

进入编码前请确认以下三项：

1. 结束原因命名采用 `SshSessionEnd::{SessionFinished, TransportClosed, WorkerShutdown, RemocClosed}`。
2. `run_session`、`run_pty`、`run_piped` 直接增加末尾 `CancellationToken` 参数，不为一个字段新增 context/options struct。
3. 异常断开使用 `SIGHUP 1s -> SIGTERM 1s -> SIGKILL + wait`，整体目标 5 秒内释放。

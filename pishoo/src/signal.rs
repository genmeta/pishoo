use std::process;

use gateway::error::Whatever;
use snafu::{ResultExt, whatever};
use tokio::{
    fs,
    io::{self, AsyncWriteExt},
    signal::unix::{SignalKind, signal},
};

use crate::SignalType;

/// Signal received by the root supervisor process.
///
/// Represents every Unix signal the root listens for. The root is responsible
/// for forwarding the appropriate signal to workers and managing its own
/// lifecycle (shutdown vs. stay-alive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSignal {
    /// SIGTERM — fast shutdown.
    SigTerm,
    /// SIGINT — fast shutdown (Ctrl+C).
    SigInt,
    /// SIGQUIT — graceful shutdown.
    SigQuit,
    /// SIGHUP — reload (forward to workers, root stays alive).
    SigHup,
    /// SIGUSR1 — reopen logs (root reopens its own, then forwards to workers).
    SigUsr1,
    /// SIGCHLD — a child process has exited.
    SigChld,
}

pub async fn send_signal(pid_file: &str, signal_type: SignalType) -> Result<(), Whatever> {
    use nix::{sys::signal::Signal, unistd::Pid};
    // 读取 PID 文件
    let pid_str = fs::read_to_string(pid_file)
        .await
        .whatever_context(format!("failed to read pid file at `{pid_file}`",))?;

    let pid = pid_str
        .trim()
        .parse::<i32>()
        .whatever_context(format!("invalid pid in file `{pid_file}`"))?;

    // 根据信号类型发送对应的系统信号
    let signal = match signal_type {
        SignalType::Stop => Signal::SIGTERM,
        SignalType::Quit => Signal::SIGQUIT,
        SignalType::Reopen => Signal::SIGUSR1,
        SignalType::Reload => Signal::SIGHUP,
    };

    // 发送信号
    nix::sys::signal::kill(Pid::from_raw(pid), signal)
        .whatever_context(format!("failed to send {signal} signal to process {pid}"))?;

    tracing::info!(%signal, pid, "sent signal to process");
    Ok(())
}

/// Long-lived signal listener created once and reused across the main loop.
///
/// This avoids re-creating signal streams on every iteration which would open a
/// window where signals arriving between the drop of old streams and the
/// creation of new ones are silently lost (e.g. Ctrl+C during reload).
pub struct RootSignalHandler {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    usr1: tokio::signal::unix::Signal,
    child: tokio::signal::unix::Signal,
}

impl RootSignalHandler {
    pub fn new() -> Result<Self, Whatever> {
        Ok(Self {
            term: signal(SignalKind::terminate())
                .whatever_context("failed to create sigterm listener")?,
            int: signal(SignalKind::interrupt())
                .whatever_context("failed to create sigint listener")?,
            quit: signal(SignalKind::quit())
                .whatever_context("failed to create sigquit listener")?,
            hup: signal(SignalKind::hangup())
                .whatever_context("failed to create sighup listener")?,
            usr1: signal(SignalKind::user_defined1())
                .whatever_context("failed to create sigusr1 listener")?,
            child: signal(SignalKind::child())
                .whatever_context("failed to create sigchld listener")?,
        })
    }

    pub async fn wait(&mut self) -> RootSignal {
        tokio::select! {
            _ = self.term.recv() => {
                tracing::info!("received sigterm signal");
                RootSignal::SigTerm
            }
            _ = self.int.recv() => {
                tracing::info!("received sigint signal");
                RootSignal::SigInt
            }
            _ = self.quit.recv() => {
                tracing::info!("received sigquit signal");
                RootSignal::SigQuit
            }
            _ = self.hup.recv() => {
                tracing::info!("received sighup signal");
                RootSignal::SigHup
            }
            _ = self.usr1.recv() => {
                tracing::info!("received sigusr1 signal");
                RootSignal::SigUsr1
            }
            _ = self.child.recv() => {
                tracing::debug!("received sigchld signal");
                RootSignal::SigChld
            }
        }
    }
}

// 写入 PID 文件（仅 Unix）
pub async fn init_pid_file(pid_file_path: &str) -> Result<(), Whatever> {
    let pid = process::id().to_string();
    let mut pid_file = match fs::File::create_new(pid_file_path).await {
        Ok(pid_file) => pid_file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            handle_existing_pid_file(pid_file_path, error).await?
        }
        Err(error) => {
            return whatever!(
                Err(error),
                "failed to create new pid file at `{pid_file_path}`\n\
                    please either:\n\
                    - run as root user, or\n\
                    - change the `pid_file` directive in your config to a writable path",
            );
        }
    };

    pid_file
        .write_all(pid.as_bytes())
        .await
        .whatever_context(format!("failed to write pid to file `{pid_file_path}`"))?;
    pid_file
        .shutdown()
        .await
        .whatever_context(format!("failed to close pid file `{pid_file_path}`"))
}

// 处理已存在的 PID 文件
async fn handle_existing_pid_file(
    pid_file_path: &str,
    original_error: io::Error,
) -> Result<fs::File, Whatever> {
    use nix::unistd::Pid;

    // 读取旧的 PID
    let old_pid_str = match fs::read_to_string(pid_file_path).await {
        Ok(content) => content,
        Err(_) => {
            tracing::warn!("cannot read pid file, removing stale pid file");
            return recreate_pid_file(pid_file_path).await;
        }
    };

    // 解析 PID
    let old_pid = match old_pid_str.trim().parse::<i32>() {
        Ok(pid) => pid,
        Err(_) => {
            tracing::warn!("pid file contains invalid pid, removing stale pid file");
            return recreate_pid_file(pid_file_path).await;
        }
    };

    // 检查进程是否还在运行
    match nix::sys::signal::kill(Pid::from_raw(old_pid), None) {
        Ok(_) => {
            // 进程仍在运行
            whatever!(
                Err(original_error),
                "pid file `{pid_file_path}` already exists and process {old_pid} is still running\n\
- if you want to start multiple instances, please change the `pid_file` directive in your config to a different location"
            )
        }
        Err(_) => {
            // 进程不存在，删除旧的 PID 文件
            tracing::warn!(
                "pid file exists but process {old_pid} is not running, removing stale pid file"
            );
            recreate_pid_file(pid_file_path).await
        }
    }
}

// 删除并重新创建 PID 文件
async fn recreate_pid_file(pid_file_path: &str) -> Result<fs::File, Whatever> {
    fs::remove_file(pid_file_path)
        .await
        .whatever_context(format!(
            "failed to remove stale pid file at `{pid_file_path}`"
        ))?;

    fs::File::create(pid_file_path)
        .await
        .whatever_context(format!("failed to create pid file at `{pid_file_path}`"))
}

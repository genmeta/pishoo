use std::process;

use gateway::error::Whatever;
use snafu::{ResultExt, whatever};
use tokio::{
    fs,
    io::{self, AsyncWriteExt},
    signal::unix::{SignalKind, signal},
};

use crate::SignalType;
use crate::signal::ShutdownSignal;

pub async fn send_signal(pid_file: &str, signal_type: SignalType) -> Result<(), Whatever> {
    use nix::{sys::signal::Signal, unistd::Pid};
    // 读取 PID 文件
    let pid_str = fs::read_to_string(pid_file)
        .await
        .whatever_context(format!("Failed to read PID file at `{pid_file}`",))?;

    let pid = pid_str
        .trim()
        .parse::<i32>()
        .whatever_context(format!("Invalid PID in file `{pid_str}`"))?;

    // 根据信号类型发送对应的系统信号
    let signal = match signal_type {
        SignalType::Stop => Signal::SIGTERM,
        SignalType::Quit => Signal::SIGQUIT,
        SignalType::Reopen => Signal::SIGUSR1,
        SignalType::Reload => Signal::SIGHUP,
    };

    // 发送信号
    nix::sys::signal::kill(Pid::from_raw(pid), signal)
        .whatever_context(format!("Failed to send {signal} signal to process {pid}"))?;

    println!("Sent {signal} signal to process {pid}");
    Ok(())
}

pub async fn handle_signal(
) -> Result<Option<ShutdownSignal>, Whatever> {
    // 设置信号处理器（仅 Unix 可用）
    let mut term_signal =
        signal(SignalKind::terminate()).whatever_context("Failed to create SIGTERM listener")?;
    let mut int_signal =
        signal(SignalKind::interrupt()).whatever_context("Failed to create SIGINT listener")?;
    let mut quit_signal =
        signal(SignalKind::quit()).whatever_context("Failed to create SIGQUIT listener")?;
    let mut hup_signal =
        signal(SignalKind::hangup()).whatever_context("Failed to create SIGHUP listener")?;
    let mut usr1_signal = signal(SignalKind::user_defined1())
        .whatever_context("Failed to create SIGUSR1 listener")?;

    loop {
        tokio::select! {
            _ = term_signal.recv() => {
                tracing::info!(target: "signal", "Received SIGTERM signal, exiting immediately...");
                return Ok(Some(ShutdownSignal::SigTerm));
            }
            _ = int_signal.recv() => {
                tracing::info!(target: "signal", "Received SIGINT signal (Ctrl+C), exiting immediately...");
                return Ok(Some(ShutdownSignal::SigInt));
            }
            _ = quit_signal.recv() => {
                tracing::info!(target: "signal", "Received SIGQUIT signal");
            }
            _ = hup_signal.recv() => {
                tracing::info!(target: "signal", "Received SIGHUP signal");
            }
            _ = usr1_signal.recv() => {
                tracing::info!(target: "signal", "Received SIGUSR1 signal");
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
                "Failed to create new PID file at `{pid_file_path}`\n\
                    Please either:\n\
                    - Run as root user, or\n\
                    - Change the `pid_file` directive in your config to a writable path.",
            );
        }
    };

    pid_file
        .write_all(pid.as_bytes())
        .await
        .whatever_context(format!("Failed to write PID to file `{pid_file_path}`"))?;
    pid_file
        .shutdown()
        .await
        .whatever_context(format!("Failed to close PID file `{pid_file_path}`"))
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
            tracing::warn!(target: "signal", "Cannot read PID file, removing stale PID file");
            return recreate_pid_file(pid_file_path).await;
        }
    };

    // 解析 PID
    let old_pid = match old_pid_str.trim().parse::<i32>() {
        Ok(pid) => pid,
        Err(_) => {
            tracing::warn!(target: "signal", "PID file contains invalid PID, removing stale PID file");
            return recreate_pid_file(pid_file_path).await;
        }
    };

    // 检查进程是否还在运行
    match nix::sys::signal::kill(Pid::from_raw(old_pid), None) {
        Ok(_) => {
            // 进程仍在运行
            whatever!(
                Err(original_error),
                "PID file `{pid_file_path}` already exists and process {old_pid} is still running.\n\
- If you want to start multiple instances, please change the `pid_file` directive in your config to a different location."
            )
        }
        Err(_) => {
            // 进程不存在，删除旧的 PID 文件
            tracing::warn!(target: "signal", "PID file exists but process {old_pid} is not running, removing stale PID file");
            recreate_pid_file(pid_file_path).await
        }
    }
}

// 删除并重新创建 PID 文件
async fn recreate_pid_file(pid_file_path: &str) -> Result<fs::File, Whatever> {
    fs::remove_file(pid_file_path)
        .await
        .whatever_context(format!(
            "Failed to remove stale PID file at `{pid_file_path}`"
        ))?;

    fs::File::create(pid_file_path)
        .await
        .whatever_context(format!("Failed to create PID file at `{pid_file_path}`"))
}

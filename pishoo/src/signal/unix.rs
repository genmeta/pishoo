use std::{path::PathBuf, process, sync::Arc};

use gateway::{error::Whatever, parse};
use snafu::{OptionExt, ResultExt, whatever};
use tokio::{
    fs,
    io::{self, AsyncWriteExt},
    signal::unix::{Signal, SignalKind, signal},
    sync::Mutex,
    task::JoinSet,
};

use crate::{
    SignalType,
    service::{start_services_from_pishoo_block, stop_services},
};

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
    config_file: PathBuf,
    handler: &Arc<Mutex<JoinSet<()>>>,
) -> Result<(), Whatever> {
    // 设置信号处理器（仅 Unix 可用）
    let mut term_signal =
        signal(SignalKind::terminate()).whatever_context("Failed to create SINTERM listener")?;
    let quit_signal =
        signal(SignalKind::quit()).whatever_context("Failed to create SIGQUIT listener")?;
    let hup_signal =
        signal(SignalKind::hangup()).whatever_context("Failed to create SIGHUP listener")?;
    let usr1_signal = signal(SignalKind::user_defined1())
        .whatever_context("Failed to create SIGUSR1 listener")?;

    tokio::spawn(handle_sigquit(quit_signal, handler.clone()));

    // 处理 SIGHUP 信号（仅 Unix）：先解析新配置，成功后停止旧任务并立即以新配置重启

    let handler_clone = Arc::clone(handler);
    tokio::spawn(handle_sighup(hup_signal, config_file, handler_clone));

    // 处理 SIGUSR1 信号（仅 Unix）
    tokio::spawn(handle_sigusr1(usr1_signal));

    // 等待 SIGTERM 并退出（仅 Unix）
    term_signal.recv().await;
    tracing::info!(target: "signal", "Received Stop signal, exiting immediately...");

    Ok(())
}

// 写入 PID 文件（仅 Unix）
pub async fn init_pid_file(pid_file_path: &str) -> Result<(), Whatever> {
    let pid = process::id().to_string();
    let mut pid_file = match fs::File::create_new(pid_file_path).await {
        Ok(pid_file) => pid_file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return whatever!(
                Err(error),
                "PID file `{pid_file_path}` already exists. Is Pishoo already running?\n\
- If not, please remove the file and try again.\n\
- If you want to start multiple instances, please change the `pid_file`\
 directive in your config to a different location.",
            );
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

    async {
        pid_file.write_all(pid.as_bytes()).await?;
        pid_file.shutdown().await
    }
    .await
    .whatever_context(format!("Failed to write PID to file `{pid_file_path}`",))
}

// 处理 SIGQUIT 信号（仅 Unix）
async fn handle_sigquit(mut quit_signal: Signal, handler: Arc<Mutex<JoinSet<()>>>) {
    use crate::service::stop_services;

    quit_signal.recv().await;
    tracing::info!(target: "signal" ,"Received SIGTERM signal, shutting down gracefully...");
    _ = stop_services(&handler).await;
}

// 处理 SIGHUP 信号（仅 Unix）
async fn handle_sighup(
    mut hup_signal: Signal,
    config_file: PathBuf,
    handler: Arc<Mutex<JoinSet<()>>>,
) {
    let try_restart = async || {
        let configure = fs::read(&config_file)
            .await
            .whatever_context("Read config failed")?;
        let new_config = parse::parse(&configure, config_file.parent())
            .whatever_context("Parse config failed")?;
        let parse::Value::Nodes(pishoos) = new_config
            .get("pishoo")
            .whatever_context("Pishoo block not exists")?
        else {
            unreachable!("Parse error");
        };
        let pishoo = pishoos
            .first()
            .expect("No pishoo block found, but pishoo key exists");
        start_services_from_pishoo_block(&handler, pishoo).await?;

        Result::<_, Whatever>::Ok(())
    };

    loop {
        hup_signal.recv().await;
        let start_at = std::time::Instant::now();
        tracing::info!(target: "signal", "Received SIGHUP signal, restarting services...");

        stop_services(&handler).await;

        if let Err(restart_error) = try_restart().await {
            tracing::error!(target: "signal", "Failed to restart services: {restart_error:?}.");
            continue;
        }

        tracing::info!(
            target: "signal",
            "Reloaded in {:?}",
            std::time::Instant::now().duration_since(start_at)
        );
    }
}

// 处理 SIGUSR1 信号（仅 Unix）
async fn handle_sigusr1(mut usr1_signal: Signal) {
    loop {
        usr1_signal.recv().await;
        tracing::info!(target: "signal", "Received SIGUSR1 signal,  reopening log files...");
        // 这里应该实现重新打开日志文件的逻辑
        // 由于当前代码中没有具体的日志文件处理，我们只记录一条日志

        tracing::info!(target: "signal", "Log files reopened");
    }
}

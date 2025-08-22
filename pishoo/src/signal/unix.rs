use std::{fs, io, path::PathBuf, process, sync::Arc};

use anyhow::{Context, Result};
use tokio::{
    signal::unix::{Signal, SignalKind, signal},
    sync::Mutex,
    task::JoinSet,
};

pub fn send_signal(pid_file: &str, signal_type: &str) -> Result<()> {
    use nix::{sys::signal::Signal, unistd::Pid};
    // 读取 PID 文件
    let pid_str = fs::read_to_string(pid_file)
        .with_context(|| format!("Failed to read PID file: {}", pid_file))?;

    let pid = pid_str
        .trim()
        .parse::<i32>()
        .with_context(|| format!("Invalid PID in file: {}", pid_str))?;

    // 根据信号类型发送对应的系统信号
    let signal = match signal_type {
        "stop" => Signal::SIGTERM,
        "quit" => Signal::SIGQUIT,
        "reopen" => Signal::SIGUSR1,
        "reload" => Signal::SIGHUP,
        _ => return Err(anyhow::anyhow!("Unknown signal type: {}", signal_type)),
    };

    // 发送信号
    nix::sys::signal::kill(Pid::from_raw(pid), signal)
        .with_context(|| format!("Failed to send {} signal to process {}", signal_type, pid))?;

    println!("Sent {} signal to process {}", signal_type, pid);
    Ok(())
}

pub async fn handle_signal(config_file: PathBuf, handler: &Arc<Mutex<JoinSet<()>>>) -> Result<()> {
    // 设置信号处理器（仅 Unix 可用）
    let mut term_signal = signal(SignalKind::terminate())?;
    let quit_signal = signal(SignalKind::quit())?;
    let hup_signal = signal(SignalKind::hangup())?;
    let usr1_signal = signal(SignalKind::user_defined1())?;

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
pub fn init_pid_file(pid_file: &str) -> Result<()> {
    let pid = process::id().to_string();
    match fs::write(pid_file, &pid) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Err(anyhow::anyhow!(
            "Failed to write PID file due to insufficient permissions: {}\n\
                Please either:\n\
                1. Run as root user, or\n\
                2. Change the 'pid' path in your config to a writable location",
            pid_file
        )),
        Err(e) => Err(e).with_context(|| format!("Failed to write PID file: {}", pid_file)),
    }
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
    loop {
        use gateway::parse::{self, Value};

        use crate::service::stop_services;

        hup_signal.recv().await;
        let start_at = std::time::Instant::now();
        tracing::info!(target: "signal", "Received SIGHUP signal, reloading configuration...");

        // 1) 读取并解析新配置
        let configure = match std::fs::read(&config_file) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(target: "signal", "Reload read config failed: {}", e);
                continue;
            }
        };
        let new_config = match parse::parse(&configure, config_file.parent()) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::error!(target: "signal", "Reload parse config failed: {}", e);
                continue;
            }
        };

        // 2) 拿到 pishoo 根、servers、proxys
        let pishoo = if let Some(Value::Nodes(pishoo)) = new_config.get("pishoo") {
            Arc::clone(pishoo.first().unwrap())
        } else {
            tracing::error!(target: "signal", "Reload failed: pishoo block not found");
            continue;
        };

        let proxys = if let Some(Value::Nodes(p)) = pishoo.get("proxy") {
            p.clone()
        } else {
            Vec::new()
        };
        let servers = if let Some(Value::Nodes(s)) = pishoo.get("server") {
            s.clone()
        } else {
            Vec::new()
        };

        tracing::info!(
            "reload parse ok: servers={}, proxys={}",
            servers.len(),
            proxys.len()
        );

        // 3) 停止旧任务
        if let Err(e) = stop_services(&handler).await {
            tracing::error!(target: "signal", "Reload stop_services error: {}", e);
            // 不阻断后续启动，继续尝试启动新服务以减少停机
        }

        // 4) 启动新任务
        {
            use crate::start_services;

            let mut h = handler.lock().await;
            start_services(&mut h, &servers, &proxys);
        }

        tracing::info!(
            "Configuration reloaded in {:?}",
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

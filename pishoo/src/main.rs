use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::{Parser, command};
use gateway::parse::{self, Value};
use tokio::{sync::broadcast, task::JoinSet};

use crate::service::start_services;

mod service;
mod signal;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(
        short,
        default_value = "/etc/pishoo/pishoo.conf",
        help = "set configuration file (default: /etc/pishoo/pishoo.conf)"
    )]
    config_file: PathBuf,
    #[arg(
        short,
        default_value = None,
        help = "set configuration file (default: stderr)"
    )]
    error_output: Option<PathBuf>,
    #[arg(
        short,
        default_value = None,
        value_parser = clap::builder::PossibleValuesParser::new(["stop", "quit", "reopen", "reload"]),
        help = "send signal to a master process (-s only on Linux/macOS)"
    )]
    signal: Option<String>,
    #[arg(short, default_value_t = false, help = "test configuration and exit")]
    test_config: bool,
    #[arg(short = 'g', help = "set global directives out of configuration file")]
    directives: Vec<String>,
}

// TODO: multi-thread async runtime
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 写入 PID 文件 (仅 Unix 使用，Windows 屏蔽不写入)
    // TODO 从配置文件读取 pid 文件路径
    #[cfg(unix)]
    let pid_file: &str = "/var/run/pishoo.pid";
    #[cfg(windows)]
    let pid_file: &str = "NUL"; // 占位，后续被 cfg(windows) 路径屏蔽，不会使用

    // 处理信号发送
    if let Some(signal_type) = &args.signal {
        return signal::send_signal(pid_file, signal_type);
    }

    #[cfg(unix)]
    signal::init_pid_file(pid_file)?;

    // TODO 将日志存储到 /var/pishoo/pishoo.log

    #[cfg(not(feature = "console_subscriber"))]
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .init();

    #[cfg(feature = "console_subscriber")]
    console_subscriber::init();
    tracing::info!("Tracing initialized.");

    let config_file = args.config_file;
    let configure = std::fs::read(&config_file).expect("Failed to read configuration file");
    let config =
        parse::parse(&configure, config_file.parent()).expect("Failed to parse configuration file");

    let pishoo = if let Some(Value::Nodes(pishoo)) = config.get("pishoo") {
        Arc::clone(pishoo.first().unwrap())
    } else {
        return Err(anyhow::anyhow!("pishoo block not found"));
    };

    let proxys = if let Some(Value::Nodes(pishoo)) = pishoo.get("proxy") {
        pishoo
    } else {
        &Vec::new()
    };

    let servers = if let Some(Value::Nodes(servers)) = pishoo.get("server") {
        servers
    } else {
        &Vec::new()
    };

    // If in test configuration mode, return after validating the configuration
    if args.test_config {
        println!("Configuration test successful");
        println!("Configuration file: {}", config_file.display());

        println!("Number of servers: {}", servers.len());
        println!("Number of proxies: {}", proxys.len());

        return Ok(());
    }

    // 将 JoinSet、配置路径、停止通道等编排到可在 SIGHUP 中访问的结构中
    let handler = Arc::new(tokio::sync::Mutex::new(JoinSet::new()));
    let (shutdown_tx, _) = broadcast::channel::<()>(8);

    // 启动初始服务
    {
        let mut h = handler.lock().await;
        start_services(&mut h, servers, proxys, Some(shutdown_tx.subscribe()));
    }

    signal::handle_signal(&shutdown_tx, config_file, &handler).await
}

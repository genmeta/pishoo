use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use gateway::{error::Whatever, parse::Value};
use snafu::{OptionExt, ResultExt};
use tokio::{fs, task::JoinSet};

use crate::service::start_services_from_pishoo_block;

mod config;
mod service;
mod signal;

#[cfg(unix)]
const PID_FILE_DEFAULT: &str = "/var/run/pishoo.pid";
#[cfg(windows)]
const PID_FILE_DEFAULT: &str = "NUL"; // 占位，后续被 cfg(windows) 路径屏蔽，不会使用

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Set configuration file
    #[arg(short, default_value = "/etc/pishoo/pishoo.conf")]
    config_file: PathBuf,
    // /// Set error log file [default: stderr]
    // #[arg(short, default_value = None)]
    // error_output: Option<PathBuf>,
    /// Send signal to a master process (only on Linux/MacOS)
    #[arg(short, default_value = None)]
    signal: Option<SignalType>,
    /// Test configuration and exit
    #[arg(short, default_value_t = false)]
    test_config: bool,
    // /// Set global directives out of configuration file
    // #[arg(short = 'g')]
    // directives: Vec<String>,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum SignalType {
    Stop,
    Quit,
    Reopen,
    Reload,
}

#[tokio::main]
#[snafu::report]
async fn main() -> Result<(), Whatever> {
    let args = Args::parse();

    // TODO 将日志存储到 /var/pishoo/pishoo.log

    #[cfg(not(feature = "console_subscriber"))]
    {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(tracing::Level::INFO.into())
                    .from_env_lossy(),
            )
            .with_ansi(atty::is(atty::Stream::Stdout));
        #[cfg(debug_assertions)]
        let subscriber = subscriber.with_file(true).with_line_number(true);
        subscriber.init();
    }

    #[cfg(feature = "console_subscriber")]
    console_subscriber::init();

    let config_file = args.config_file;

    let config = fs::read(&config_file).await.whatever_context(format!(
        "Failed to read configuration file at `{}`",
        config_file.display()
    ))?;
    let config = gateway::parse::parse(&config, config_file.parent()).whatever_context(format!(
        "Failed to parse configuration file at `{}`",
        config_file.display()
    ))?;

    let pishoo = if let Value::Nodes(pishoo) = config.get("pishoo").whatever_context(format!(
        "Pishoo block not found in configuration file `{}`",
        config_file.display()
    ))? {
        pishoo
            .first()
            .expect("No pishoo block found, but pishoo key exists")
    } else {
        unreachable!("Parse error")
    };

    if args.test_config {
        tracing::info!(
            target: "config",
            "Configuration file `{}` syntax is ok",
            config_file.display()
        );
        return Ok(());
    }

    let pid_file = if let Some(Value::String(pid_file)) = pishoo.get("pid") {
        pid_file
    } else {
        PID_FILE_DEFAULT
    };

    if let Some(signal) = args.signal {
        return signal::send_signal(pid_file, signal).await;
    }

    let handler = Arc::new(tokio::sync::Mutex::new(JoinSet::new()));
    start_services_from_pishoo_block(&handler, pishoo).await?;
    signal::handle_signal(config_file, &handler).await?;
    _ = fs::remove_file(pid_file).await;
    Ok(())
}

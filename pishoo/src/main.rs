use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::{Parser, command};
use gateway::{
    forward,
    parse::{self, Value},
    reverse,
};
use qdns::Dns;
use tokio::task::JoinSet;

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
        help = "send signal to a master process"
    )]
    signal: Option<String>,
    #[arg(short, default_value_t = false, help = "test configuration and exit")]
    test_config: bool,
    #[arg(
        short,
        default_value_t = false,
        help = "suppress non-error messages during configuration testing"
    )]
    quiet: bool,
    #[arg(short = 'g', help = "set global directives out of configuration file")]
    directives: Vec<String>,
    // TODO
    // #[arg(
    //     default_value_t = false,
    //     help = "enable logging, write to /var/pishoo/pishoo.log"
    // )]
    // log: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

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
    let configure = std::fs::read(&config_file)?;
    let config = parse::parse(&configure, config_file.parent())?;

    // TODO 对于绑定到 [::]:0 的监听, 应该进行特殊操作, 每个 server 都单独绑定到 不同端口 上

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

    // 启动自动 DNS 汇报
    let dns = Dns::default();
    dns.spawn_publish();

    let mut handler = JoinSet::new();

    handler.spawn(reverse::serve(servers.clone()));

    for proxy in proxys {
        handler.spawn(forward::serve(Arc::clone(proxy)));
    }

    handler.join_all().await;

    Ok(())
}

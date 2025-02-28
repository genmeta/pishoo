use std::path::PathBuf;

use clap::{Parser, command};
use futures::future::join_all;
use gateway::{
    ForwardServer, ReverseServer,
    parse::gateway::{Gateway, Record, parse_gateway},
};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use tracing::{error, info};

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
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .init();
    tracing::info!("Tracing initialized.");

    // 初始化TLS
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config_file = args.config_file;
    let configure = std::fs::read(&config_file)?;
    let mut gateway = Gateway::default();
    if let Ok(res) = Directive::<Nginx>::parse(&configure) {
        for mut directive in res {
            let path = config_file
                .parent()
                .expect("config path should have a parent");
            directive.resolve_include(path)?;
            if directive.name == "pishoo" {
                if let Some(children) = directive.children {
                    gateway = parse_gateway(children).inspect_err(|e| error!("{:?}", e))?;
                    break;
                }
            }
        }
    }

    // TODO 对于绑定到 [::]:0 的监听, 应该进行特殊操作, 每个 server 都单独绑定到 不同端口 上

    let mut handlers = Vec::new();
    for (bind, record) in gateway.records {
        let handle = tokio::spawn({
            async move {
                info!("Launching server on {}, servers: {:#?}", bind, record);
                match record {
                    Record::Reverse(servers) => {
                        ReverseServer::serve(bind, servers).await?;
                    }
                    Record::Forward(_server) => {
                        ForwardServer::serve(bind).await?;
                    }
                }

                Ok::<_, Box<dyn std::error::Error + 'static + Send + Sync>>(())
            }
        });
        handlers.push(handle);
    }

    join_all(handlers).await;

    Ok(())
}

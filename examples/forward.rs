use futures::future::join_all;
use gateway::{
    forward,
    parse::gateway::{Gateway, Record, parse_gateway},
};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .init();
    tracing::info!("Tracing initialized.");

    // 初始化TLS
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().collect();

    // 检查是否至少有一个参数传入
    let config_file = if args.len() > 1 {
        &args[1]
    } else {
        eprintln!("Usage: {} <config file>", args[0]);
        std::process::exit(1);
    };
    let config_file = std::path::Path::new(config_file);

    let configure = std::fs::read(config_file)?;
    let mut gateway = Gateway::default();
    if let Ok(res) = Directive::<Nginx>::parse(&configure) {
        for mut directive in res {
            let path = config_file
                .parent()
                .expect("config path should have a parent");
            directive.resolve_include(path)?;
            if directive.name == "pishoo" {
                if let Some(children) = directive.children {
                    gateway =
                        parse_gateway(children).inspect_err(|e| error!("parse error: {}", e))?;
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
                if let Record::Forward(server) = record {
                    forward::serve(server).await?;
                }

                Ok::<_, Box<dyn std::error::Error + 'static + Send + Sync>>(())
            }
        });
        handlers.push(handle);
    }

    handlers.push(tokio::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            info!("I'm still alive");
        }
    }));

    join_all(handlers).await;
    Ok(())
}

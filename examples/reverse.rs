use std::{env, path::PathBuf};

use futures::future::join_all;
use gateway::{
    forward::ForwardServer,
    parse::gateway::{Gateway, Record, parse_gateway},
    reverse::ReverseServer,
};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .init();
    tracing::info!("Tracing initialized.");

    // 初始化TLS
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config_path = if let Some(config_path) = env::args().nth(1) {
        info!("config_path: {}", config_path);
        config_path
    } else {
        error!("config_path not provided");
        return Ok(());
    };
    let config_path = PathBuf::from(config_path);

    let data = std::fs::read(&config_path)?;

    let mut gateway = Gateway::default();

    if let Ok(res) = Directive::<Nginx>::parse(&data) {
        for mut directive in res {
            let path = config_path
                .parent()
                .expect("config path should have a parent");
            directive.resolve_include(path)?;
            if directive.name == "http3" {
                if let Some(children) = directive.children {
                    gateway = parse_gateway(children)?;
                    // println!("{:#?}", gateway);
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
                // TODO 提前探测 nat 类型, 不用等到连接时
                // let addr_registry = get_or_create_addr_rigistery(bind).unwrap();
                // let _outer = addr_registry.outer_addr().await.unwrap();
                // let _nat_type = addr_registry.nat_type().await.unwrap();
                match record {
                    Record::Reverse(servers) => {
                        ReverseServer::serve(bind, servers).await?;
                    }
                    Record::Forward(_server) => {
                        ForwardServer::serve(bind).await;
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

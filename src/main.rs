#![feature(slice_pattern)]

use std::{
    env,
    path::PathBuf,
};

use forward::ForwardServer;
use futures::future::join_all;
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use parse::gateway::{Gateway, Record, parse_gateway};
use reverse::ReverseServer;
use tracing::{error, info};

mod common;
mod config;
mod dns;
mod error;
mod forward;
mod parse;
mod reverse;
mod support;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::init().await;

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
                match record {
                    Record::Reverse(servers) => {
                        ReverseServer::serve(bind, servers).await?;
                    }
                    Record::Forward(server) => {
                        ForwardServer::serve(bind, server).await;
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

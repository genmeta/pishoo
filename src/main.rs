#![feature(slice_pattern)]

use std::{collections::HashMap, path::PathBuf};

use futures::future::join_all;
use http::{HttpServer, http3::H3Server};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use parse::{
    gateway::{Gateway, parse_gateway},
    server::Server,
    version::HttpVersion,
};
use tracing::info;

mod common;
mod config;
mod error;
mod http;
mod parse;
mod support;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::init().await;

    let data = std::fs::read("config/test.conf")?;

    let mut gateway = Gateway::new();

    if let Ok(res) = Directive::<Nginx>::parse(&data) {
        for mut directive in res {
            let path = PathBuf::from("config");
            directive.resolve_include(&path)?;
            if directive.name == "http3" {
                if let Some(children) = directive.children {
                    gateway = parse_gateway(children)?;
                    println!("{:#?}", gateway);
                }
            }
        }
    }

    // TODO 对于绑定到 [::]:0 的监听, 应该进行特殊操作, 每个 server 都单独绑定到 不同端口 上

    let mut handlers = Vec::new();
    for (addr, record) in gateway.records {
        let handle = tokio::spawn({
            async move {
                info!("Launching server on {}, servers: {:#?}", addr, record);

                let grouped: HashMap<HttpVersion, Vec<Server>> =
                    record
                        .into_iter()
                        .fold(HashMap::new(), |mut acc, (_key, server)| {
                            acc.entry(server.version).or_default().push(server);
                            acc
                        });

                for (version, servers) in grouped {
                    match version {
                        HttpVersion::HTTP1 => {
                            HttpServer::serve(addr, servers).await;
                        }
                        HttpVersion::HTTP3 => {
                            H3Server::serve(addr, servers).await?;
                        }
                        _ => {}
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

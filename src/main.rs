#![feature(slice_pattern)]

use std::path::PathBuf;

use futures::future::join_all;
use http::http3::H3Server;
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use parse::{
    gateway::{Gateway, parse_gateway},
    version::HttpVersion,
};
use tracing::info;

mod client;
mod common;
mod config;
mod error;
mod handle;
mod http;
mod parse;

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

    // TODO 对于绑定到 [::]:0 的监听, 应该进行特殊操作, 每个 server 都单独绑定到 server 上

    let mut handlers = Vec::new();
    for (addr, record) in gateway.records {
        let handle = tokio::spawn({
            async move {
                info!("Launching server on {}, servers: {:#?}", addr, record);

                for (version, servers) in record {
                    match version {
                        HttpVersion::HTTP1 => http::serve(addr, servers).await,
                        HttpVersion::HTTP2 => {
                            // TODO
                        }
                        HttpVersion::HTTP3 => {
                            let mut server = H3Server::new(addr, servers).unwrap();
                            server.launch().await.unwrap();
                        }
                    }
                }
            }
        });
        handlers.push(handle);
    }

    join_all(handlers).await;

    Ok(())
}

#![feature(slice_pattern)]

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, LazyLock},
};

use dashmap::DashMap;
use forward::ForwardServer;
use futures::future::join_all;
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use parse::gateway::{Gateway, Record, parse_gateway};
use qtraversal::AddressRegisty;
use reverse::ReverseServer;
use tracing::info;

static ADDRESSES: LazyLock<DashMap<SocketAddr, AddressRegisty>> = LazyLock::new(DashMap::new);
static AGENT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 12, 74, 4)), 20002);

mod common;
mod config;
mod error;
mod forward;
mod parse;
mod reverse;
mod support;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::init().await;

    let data = std::fs::read("config/test.conf")?;

    let mut gateway = Gateway::default();

    if let Ok(res) = Directive::<Nginx>::parse(&data) {
        for mut directive in res {
            let path = PathBuf::from("config");
            directive.resolve_include(&path)?;
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
        let addr_registry = match ADDRESSES.entry(bind) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let registry = AddressRegisty::new(bind, AGENT)?;
                entry.insert(registry.clone());
                registry
            }
        };
        let handle = tokio::spawn({
            async move {
                info!("Launching server on {}, servers: {:#?}", bind, record);
                match record {
                    Record::Forward(servers) => {
                        ForwardServer::serve(bind, servers, addr_registry).await?;
                    }
                    Record::Reverse(server) => {
                        ReverseServer::serve(bind, server, addr_registry).await;
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

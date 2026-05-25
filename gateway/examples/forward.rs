use std::sync::Arc;

use dhttp::endpoint::Endpoint;
use gateway::{forward, parse};
use snafu::{FromString, OptionExt, Whatever, whatever};
use tokio::task::JoinSet;
use tracing::Instrument;

#[tokio::main]
#[snafu::report]
async fn main() -> Result<(), Whatever> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .init();
    tracing::info!("tracing initialized");

    let args: Vec<String> = std::env::args().collect();

    // 检查是否至少有一个参数传入
    let config_file = if args.len() > 1 {
        &args[1]
    } else {
        eprintln!("usage: {} <config file>", args[0]);
        std::process::exit(1);
    };
    let config_file = std::path::Path::new(config_file);
    let registry = parse::default_registry();
    let config = match parse::load_config_file(
        config_file,
        &registry,
        parse::registry::BuildOptions::default(),
    )
    .await
    {
        Ok(config) => config,
        Err(failure) => {
            tracing::error!(
                error = %snafu::Report::from_error(&failure.error),
                diagnostic = %failure.diagnostic(),
                "failed to load configuration"
            );
            return Err(Whatever::with_source(
                Box::new(failure),
                "failed to load configuration".to_owned(),
            ));
        }
    };

    // TODO 对于绑定到 [::]:0 的监听, 应该进行特殊操作, 每个 server 都单独绑定到 不同端口 上

    let pishoo = config
        .root
        .children("pishoo")
        .ok()
        .and_then(|pishoo| pishoo.first())
        .whatever_context("no pishoo block found")?;

    let proxies = pishoo.children_optional("proxy").to_vec();
    if proxies.is_empty() {
        whatever!("no proxy found in pishoo configuration");
    }

    // Build a DHTTP endpoint for outbound proxying.
    let client = Arc::new(Endpoint::builder().build().await);

    let mut handler = JoinSet::new();

    for proxy in proxies {
        let span = tracing::info_span!("forward_example_proxy");
        let client = client.clone();
        handler.spawn(
            async move {
                match forward::serve(proxy, client).await {
                    Ok((bind_addr, forward_proxy)) => {
                        tracing::info!(%bind_addr, "forward proxy started");
                        if let Err(error) = forward_proxy.await {
                            tracing::error!(error = %snafu::Report::from_error(&error), "forward proxy failed");
                        }
                    }
                    Err(launch_error) => {
                        tracing::error!(error = %snafu::Report::from_error(&launch_error), "failed to launch forward proxy");
                    }
                }
            }
            .instrument(span),
        );
    }
    handler.join_all().await;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tracing::info!("still running");
    }
}

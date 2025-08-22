use std::sync::Arc;

use anyhow::{Result, bail};
use gateway::{
    forward,
    parse::{self, Value},
};
use tokio::task::JoinSet;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .init();
    tracing::info!("Tracing initialized.");

    let args: Vec<String> = std::env::args().collect();

    // 检查是否至少有一个参数传入
    let config_file = if args.len() > 1 {
        &args[1]
    } else {
        eprintln!("Usage: {} <config file>", args[0]);
        std::process::exit(1);
    };
    let config_file = std::path::Path::new(config_file);
    let configure = std::fs::read(config_file).unwrap();
    let config = parse::parse(&configure, config_file.parent())?;

    // TODO 对于绑定到 [::]:0 的监听, 应该进行特殊操作, 每个 server 都单独绑定到 不同端口 上

    let pishoo = if let Some(Value::Nodes(pishoo)) = config.get("pishoo") {
        Arc::clone(pishoo.first().unwrap())
    } else {
        bail!("Invalid pishoo");
    };

    let Some(Value::Nodes(proxies)) = pishoo.get("proxy").cloned() else {
        bail!("No proxy found in pishoo configuration");
    };

    let mut handler = JoinSet::new();

    for proxy in proxies {
        handler.spawn(async move {
            match forward::serve(proxy).await {
                Ok((bind_addr, forward_proxy)) => {
                    tracing::info!(target: "forward", "Forward proxy started at {bind_addr}", );
                    if let Err(error) = forward_proxy.await {
                        tracing::error!(target: "forward", "Forward proxy error: {error:?}", );
                    }
                }
                Err(launch_error) => {
                    tracing::error!(target: "forward", "Failed to launch forward proxy: {launch_error:?}", );
                }
            };
        });
    }
    handler.join_all().await;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tracing::info!("still running");
    }
}

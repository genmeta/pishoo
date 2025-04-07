use std::sync::Arc;

use anyhow::Result;
use gateway::{
    forward,
    new_parse::{self, Value},
};
use tokio::task::JoinSet;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
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
    let config = new_parse::parse(&configure, config_file.parent().unwrap())?;

    // TODO 对于绑定到 [::]:0 的监听, 应该进行特殊操作, 每个 server 都单独绑定到 不同端口 上

    let pishoo = if let Some(Value::Nodes(pishoo)) = config.get("pishoo") {
        Arc::clone(pishoo.first().unwrap())
    } else {
        return Err(anyhow::anyhow!("Invalid pishoo"));
    };

    let proxys = if let Some(Value::Nodes(pishoo)) = pishoo.get("proxy") {
        pishoo
    } else {
        &Vec::new()
    };

    let mut handler = JoinSet::new();
    for proxy in proxys {
        handler.spawn(forward::serve(Arc::clone(proxy)));
    }
    handler.join_all().await;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        tracing::info!("still running");
    }
}

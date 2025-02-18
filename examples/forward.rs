use gateway::ForwardServer;
use tracing::info;

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

    ForwardServer::serve("192.168.31.86:5379".parse().unwrap()).await?;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        info!("still running");
    }
}

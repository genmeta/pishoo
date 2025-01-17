/// 初始化服务
pub async fn init() {
    // 初始化日志
    tracing();
    // 初始化TLS
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// 初始化日志
fn tracing() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_file(true)
        .with_line_number(true)
        .init();
    tracing::info!("Tracing initialized.");
}

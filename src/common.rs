use std::{net::IpAddr, sync::OnceLock};

use reqwest::Client;

pub(crate) static H3_CLIENT: OnceLock<Client> = OnceLock::new();

/// 初始化服务
pub async fn init() {
    // 初始化日志
    tracing();
    // 初始化 h3 Client
    init_h3_client("127.0.0.1".parse().unwrap());
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

/// 初始化 h3 Client
fn init_h3_client(addr: IpAddr) {
    H3_CLIENT.get_or_init(|| crate::client::launch_h3_client(addr));
}

/// 获取 h3 Client
pub fn h3_client() -> &'static Client {
    H3_CLIENT.get().expect("h3 client not initialized")
}

#![allow(dead_code)]

use std::sync::RwLock;

use snafu::Snafu;

/// 客户端配置类型: (证书链, 私钥, 客户端名称)
type ClientConfig = (Vec<u8>, Vec<u8>, String);

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum SetClientConfigError {
    #[snafu(display("failed to acquire h3 client config write lock"))]
    WriteLockPoisoned,
}

/// 全局客户端配置存储
static CLIENT_CONFIG: RwLock<Option<ClientConfig>> = RwLock::new(None);

/// 设置客户端配置
///
/// 可以多次调用以更新证书、密钥和客户端名称。
pub fn set_client_config(
    cert_chain: Vec<u8>,
    private_key: Vec<u8>,
    client_name: String,
) -> Result<(), SetClientConfigError> {
    let mut config = CLIENT_CONFIG
        .write()
        .map_err(|_| SetClientConfigError::WriteLockPoisoned)?;
    *config = Some((cert_chain, private_key, client_name));
    Ok(())
}

/// 获取客户端配置
pub fn get_client_config() -> Option<ClientConfig> {
    CLIENT_CONFIG.read().ok().and_then(|guard| guard.clone())
}

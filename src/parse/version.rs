use tracing::info;

use crate::error::{CustomError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Default, Eq, Hash)]
pub enum HttpVersion {
    #[default]
    HTTP1,
    HTTP2,
    HTTP3,
}

pub fn parse_http_version(version: &str) -> Result<HttpVersion> {
    match version {
        "http1" => Ok(HttpVersion::HTTP1),
        "http2" => Ok(HttpVersion::HTTP2),
        "http3" => Ok(HttpVersion::HTTP3),
        _ => {
            info!("unknown directive: {}", version);
            Err(CustomError::UnknownDirective(version.to_string()))
        }
    }
}

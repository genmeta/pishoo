use bytes::Bytes;
use http::response::Builder;
use http::{StatusCode, Uri};
use std::fs;
use tracing::{error, info};

pub(super) fn handler(pattern: &str, root: &str, uri: &Uri) -> (Builder, Bytes) {
    let path = uri.path();
    let path = path.replacen(pattern, root, 1);
    info!("Serving static file: {}", path);

    match fs::read(&path) {
        Ok(buf) => {
            let builder = Builder::new().status(StatusCode::OK);
            (builder, buf.into())
        }
        Err(e) => {
            error!("Failed to read static file: {}", e);
            let builder = Builder::new().status(StatusCode::NOT_FOUND);
            (builder, Bytes::default())
        }
    }
}

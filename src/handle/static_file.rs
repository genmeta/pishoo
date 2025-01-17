use std::fs;

use bytes::Bytes;
use h3::server::RequestStream;
use h3_shim::BidiStream;
use http::{Request, Response, StatusCode};
use tracing::info;

use crate::error::Result;

pub(super) async fn handler(
    pattern: String,
    root: String,
    req: &Request<()>,
    stream: &mut RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    let path = req.uri().path();
    let path = path.replacen(&pattern, &root, 1);
    // 检测文件是否存在
    let buf = match fs::read(&path) {
        Ok(buf) => buf,
        Err(_) => {
            let resp = Response::builder().status(StatusCode::NOT_FOUND).body(())?;
            stream.send_response(resp).await?;
            return Ok(());
        }
    };

    info!("Serving static file: {}", path);

    let resp = Response::builder().status(StatusCode::OK).body(())?;

    // 发送响应
    stream.send_response(resp).await?;
    stream.send_data(buf.into()).await?;
    stream.finish().await?;
    Ok(())
}

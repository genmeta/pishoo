use bytes::{Buf, Bytes};
use h3::server::RequestStream;
use h3_shim::BidiStream;
use http::{Request, Response};
use tracing::info;

use crate::{common, error::Result};

pub(super) async fn handler(
    target: String,
    req: &Request<()>,
    stream: &mut RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    info!("request: {:?}", req);

    // 读取请求体
    let mut body = Vec::new();
    while let Some(chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.chunk());
    }

    // 构建目标 URI
    let uri = format!(
        "{}{}{}",
        target,
        req.uri().path(),
        req.uri()
            .query()
            .map(|q| format!("?{}", q))
            .unwrap_or_default()
    );

    // 构建请求
    let mut builder = common::h3_client().request(req.method().clone(), uri);
    for (key, value) in req.headers().iter() {
        builder = builder.header(key, value);
    }

    // 发送请求并获取响应
    let resp = builder.body(body).send().await?;

    // 构建响应
    let mut response_builder = Response::builder().status(resp.status());
    for (header, value) in resp.headers() {
        response_builder = response_builder.header(header, value);
    }
    let response = response_builder.body(())?;

    // 发送响应
    stream.send_response(response).await?;
    if let Ok(body) = resp.bytes().await {
        stream.send_data(body).await?;
    }
    stream.finish().await?;

    Ok(())
}

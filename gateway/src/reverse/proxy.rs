use std::{str::FromStr, sync::Arc};

use bytes::Bytes;
use futures::TryStreamExt;
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, Response, Uri, Version};
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    client::conn::http1::Builder,
};
use hyper_util::rt::TokioIo;
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tokio_stream::StreamExt;
use tracing::{debug, error, info};

use crate::{
    command,
    error::{CustomError, Result},
    h3::{H3Sink, H3Stream},
    parse::{Node, Value},
    reverse::build_error_response,
};

pub async fn handle(
    location: &Arc<Node>,
    req: Request<()>,
    receiver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    let uri = req.uri().to_string();
    // TODO 处理 proxy_set_header
    let resp = match pass(location, req, receiver).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("[Response handling][{}] Proxy request error: {:?}", uri, e);
            sender.send_response(build_error_response()).await?;
            sender.finish().await?;
            return Ok(());
        }
    };

    debug!("[Response handling][{}] Sending response", uri);
    let (mut parts, body) = resp.into_parts();

    // 添加自定义响应头字段
    command::add_header(location, &mut parts);

    // 发送响应头
    sender
        .send_response(Response::from_parts(parts, ()))
        .await?;

    debug!("[Response handling][{}] Sending response headers", uri);

    let mut body_stream =
        tokio_util::io::StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));
    let mut stream = H3Sink::new(sender);
    match tokio::io::copy(&mut body_stream, &mut stream).await {
        Ok(size) => info!("[Proxy][{uri}] Request body sent: size={size}"),
        Err(e) => error!("[Proxy][{uri}] Error sending request body: {e}"),
    }
    match stream.shutdown().await {
        Ok(()) => info!("[Proxy][{}] Request finished sent", uri),
        Err(e) => error!("[Proxy][{}] Error sending request data end: {}", uri, e),
    }
    Ok(())
}

/// 代理请求
pub async fn pass(
    location: &Node,
    req: Request<()>,
    receiver: RequestStream<RecvStream, Bytes>,
) -> Result<Response<Incoming>> {
    // 构造目标URI
    let (parts, _) = req.into_parts();
    let proxy_pass = if let Some(Value::String(proxy_pass)) = location.get("proxy_pass") {
        proxy_pass
    } else {
        return Err(CustomError::InvalidConfig(
            "Invalid proxy_pass configuration".to_string(),
        ));
    };

    let target_host = Uri::from_str(proxy_pass)
        .map_err(|_| CustomError::InvalidConfig("Invalid proxy_pass URI".to_string()))?;

    let target_uri = Uri::from_str(
        &parts
            .uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default(),
    )?;

    // 准备请求参数
    let mut new_parts = parts;
    new_parts.uri = target_uri.clone();
    new_parts.version = Version::HTTP_11;

    // 解析目标地址
    let host = target_host.host().ok_or(CustomError::MissingHost)?;
    let port = target_host.port().map(|p| p.as_u16()).unwrap_or(80);

    // 建立TCP连接
    let io = TokioIo::new(
        TcpStream::connect((host, port))
            .await
            .inspect_err(|e| error!("TCP connection error: {}:{} : {:?}", host, port, e))?,
    );

    // 创建HTTP客户端连接
    let (mut sender, conn) = Builder::new()
        .preserve_header_case(true) // 保持首字母大小写
        .title_case_headers(true) // 标题首字母大写
        .handshake(io)
        .await?;

    info!(
        "[Request processing] HTTP client connection established: {:?}",
        target_uri
    );

    // 启动连接维护任务
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            error!("[Proxy connection] Maintenance failed: {:?}", e);
        }
    });

    let stream = H3Stream::new(receiver).map(|item| item.map(Frame::data));
    // 发送代理请求
    let response = sender
        .send_request(Request::from_parts(new_parts, StreamBody::new(stream)))
        .await
        .inspect_err(|e| error!("[Request processing] Request send error: {:?}", e))?;

    debug!("[Request processing] Finished sending request body");
    Ok(response)
}

use std::{str::FromStr, sync::Arc};

use bytes::Bytes;
use futures::TryStreamExt;
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::{Request, Response, Uri, Version};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper_util::rt::TokioIo;
use snafu::{Report, ResultExt};
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tokio_stream::StreamExt;
use tracing::{debug, error};

use crate::{
    command,
    error::{Result, StreamSnafu, Whatever},
    h3::{H3Sink, H3Stream},
    parse::{Node, Value, pattern::Pattern},
    reverse::build_error_response,
};

pub async fn handle(
    location: &Arc<Node>,
    req: Request<()>,
    recver: RequestStream<RecvStream, Bytes>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    // proxy_set_header
    let req = command::proxy_set_header(location, req);
    debug!(
        target: "reverse_proxy",
        "Processing request headers: {:?}",
        req.headers()
    );

    let resp = match pass(location, req, recver).await {
        Ok(resp) => resp,
        Err(e) => {
            error!(
                target: "reverse_proxy",
                "Proxy request error: {}",
                Report::from_error(e)
            );
            let resp = build_error_response();
            sender.send_response(resp).await.context(StreamSnafu)?;
            sender.finish().await.context(StreamSnafu)?;
            return Ok(());
        }
    };

    debug!(target: "reverse_proxy", "Sending response");
    let (mut parts, body) = resp.into_parts();

    // 添加自定义响应头字段
    command::add_header(location, &mut parts);

    debug!(target: "reverse_proxy", "Sending response headers: {parts:?}");

    // 发送响应头
    let resp1 = Response::from_parts(parts, ());
    sender.send_response(resp1).await.context(StreamSnafu)?;

    let mut body_stream =
        tokio_util::io::StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));
    let mut stream = H3Sink::new(sender);
    match tokio::io::copy(&mut body_stream, &mut stream).await {
        Ok(size) => debug!(target: "reverse_proxy", "Request body sent: size={size}"),
        Err(e) => error!(
            target: "reverse_proxy",
            "Error sending request body: {}",
            Report::from_error(e)
        ),
    }
    match stream.shutdown().await {
        Ok(()) => debug!(target: "reverse_proxy", "Request finished sent"),
        Err(e) => {
            error!(target: "reverse_proxy", "Error sending request data end: {}", Report::from_error(e))
        }
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
    let Some(Value::Uri(proxy_pass)) = location.get("proxy_pass") else {
        unreachable!("proxy_pass is required for reverse proxy");
    };

    tracing::debug!(target: "reverse_proxy", "proxy_pass: {proxy_pass}");

    let mut path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();

    tracing::debug!(
        target: "reverse_proxy",
        "Original request path and query: {}",
        path_and_query
    );

    if !proxy_pass.path().eq("/") {
        // 将匹配到的路径部分替换掉原始请求路径
        let pattern = if let Value::Pattern(pattern, _) = location.value() {
            pattern
        } else {
            unreachable!("Invalid location pattern");
        };

        match pattern {
            Pattern::Exact(_) | Pattern::Regex(_) | Pattern::CRegex(_) | Pattern::Common => {
                // 精确匹配时, 直接替换整个路径, 不需要额外处理
                // 正则匹配时, 忽略 proxy_pass 的 path 部分
            }
            Pattern::Prefix(p) => {
                path_and_query = path_and_query.replace(p, proxy_pass.path());
            }
            Pattern::NormalPrefix(p) => {
                path_and_query = path_and_query.replace(p, proxy_pass.path());
            }
        }
    }

    tracing::info!(
        target: "reverse_proxy",
        "Proxying request to target URI before path replacement: {}",
        path_and_query
    );

    let target_uri = Uri::from_str(&path_and_query).whatever_context::<_, Whatever>(format!(
        "Failed to generate target URI from `{path_and_query}`"
    ))?;

    // 准备请求参数
    let mut new_parts = parts;
    new_parts.uri = target_uri.clone();
    new_parts.version = Version::HTTP_11;

    debug!(target: "reverse_proxy", "Preparing to proxy request: {new_parts:?}");

    // 解析目标地址
    // Checked in configuration parsing phase
    let host = proxy_pass.host().expect("Missing host in proxy_pass URI");
    let port = proxy_pass.port_u16().unwrap_or(80);

    // 建立TCP连接
    let io = TokioIo::new(
        TcpStream::connect((host, port))
            .await
            .whatever_context::<_, Whatever>(format!(
                "Cannot connect to target server {host}:{port}"
            ))?,
    );

    // 创建HTTP客户端连接
    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true) // 保持首字母大小写
        .title_case_headers(true) // 标题首字母大写
        .handshake(io)
        .await
        .whatever_context::<_, Whatever>("Failed to establish HTTP/1.1 client connection")?;

    debug!(
        target: "reverse_proxy",
        "HTTP client connection established: {:?}",
        target_uri
    );

    // 启动连接维护任务
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            error!(
                target: "reverse_proxy",
                "Maintenance failed: {}",
                Report::from_error(e)
            );
        }
    });

    let stream = H3Stream::new(receiver).map(|item| item.map(Frame::data));
    // 发送代理请求
    let response = sender
        .send_request(Request::from_parts(new_parts, StreamBody::new(stream)))
        .await
        .whatever_context::<_, Whatever>("Failed to send request to target")?;

    debug!(target: "reverse_proxy", "Finished sending request body");
    Ok(response)
}

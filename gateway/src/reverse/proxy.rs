use std::{str::FromStr, sync::Arc};

use futures::TryStreamExt;
use h3x::message::stream::{ReadStream, WriteStream};
use http::{Request, Response, Uri, Version};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper_util::rt::TokioIo;
use snafu::{Report, ResultExt};
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tracing::{debug, error};

use crate::{
    command,
    error::{Result, StreamSnafu, Whatever},
    parse::{Node, Value, pattern::Pattern},
    reverse::{gzip::GzipConfig, log::RequestInfo},
};

pub async fn handle(
    location: &Arc<Node>,
    req: Request<()>,
    recver: ReadStream,
    mut sender: WriteStream,
) -> Result<()> {
    let req_info = RequestInfo::from_request(&req);

    // proxy_set_header
    let req = command::proxy_set_header(location, req);

    let accept_encoding = req
        .headers()
        .get(http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let gzip = GzipConfig::from_location(location, accept_encoding);

    debug!(
        target: "reverse_proxy",
        "Processing request headers: {:?}",
        req.headers()
    );

    let resp = match pass(location, req, recver).await {
        Ok(resp) => resp,
        Err(e) => {
            let err_msg = format!("Proxy request error: {}", Report::from_error(e));
            error!(target: "reverse_proxy", "{}", err_msg);
            req_info.log_error(&err_msg).await;
            req_info.log_access(500, 0).await;

            super::send_status_and_close(sender, http::StatusCode::INTERNAL_SERVER_ERROR).await?;
            return Ok(());
        }
    };

    debug!(target: "reverse_proxy", "Sending response");
    let (mut parts, body) = resp.into_parts();

    let content_length = parts
        .headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let should_compress = gzip.should_compress(&parts, content_length);

    if should_compress {
        gzip.apply_headers(&mut parts);
    }

    // 添加自定义响应头字段
    command::add_header(location, &mut parts);

    debug!(target: "reverse_proxy", "Sending response headers: {parts:?}");

    // 发送响应头
    let status_code = parts.status;
    sender
        .send_hyper_response_parts(parts)
        .await
        .context(StreamSnafu)?;

    let body_stream =
        tokio_util::io::StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));

    let mut reader = gzip.wrap_reader(should_compress, body_stream);

    let mut writer = Box::pin(sender.into_writer());
    match tokio::io::copy(&mut reader, &mut writer).await {
        Ok(size) => {
            debug!(target: "reverse_proxy", "Request body sent: size={size}");
            req_info.log_access(status_code.as_u16(), size).await;
        }
        Err(e) => {
            let err_msg = format!("Error sending request body: {}", Report::from_error(e));
            error!(target: "reverse_proxy", "{}", err_msg);
            req_info.log_error(&err_msg).await;
        }
    }
    match writer.shutdown().await {
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
    receiver: ReadStream,
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
            Pattern::Prefix(p) | Pattern::NormalPrefix(p) => {
                if let Some(rest) = path_and_query.strip_prefix(p.as_str()) {
                    path_and_query = format!("{}{}", proxy_pass.path(), rest);
                }
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

    // 使用 h3x ReadStream 的 as_bytes_stream 将接收到的数据转换为 Stream
    let stream = receiver
        .into_bytes_stream()
        .map_ok(Frame::data)
        .map_err(std::io::Error::from);

    // 发送代理请求
    let response = sender
        .send_request(Request::from_parts(new_parts, StreamBody::new(stream)))
        .await
        .whatever_context::<_, Whatever>("Failed to send request to target")?;

    debug!(target: "reverse_proxy", "Finished sending request body");
    Ok(response)
}

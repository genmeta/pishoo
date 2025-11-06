use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use http::{Request, Response};
use http_body_util::{BodyExt, StreamBody};
use hyper::{body::Frame, server::conn::http1, service::service_fn};
use qdns::Resolvers;
use snafu::{Report, ResultExt, Whatever};
use tokio::io::{self, AsyncWriteExt};
use tracing::{Instrument, debug, error, info};

use super::BoxResponse;
use crate::{
    forward::{build_empty_response, build_error_response, tunnel_upgrade, validate_host},
    h3::{H3SendRequest, H3Sink, H3Stream},
    pool::H3ConnectionPool,
};

/// 处理普通 HTTP 请求
#[tracing::instrument(level = "info", skip_all, fields(odcid = tracing::field::Empty))]
pub async fn proxy(
    mut req: Request<hyper::body::Incoming>,
    resolvers: Resolvers,
) -> Result<BoxResponse, hyper::Error> {
    // 验证主机合法性
    let host = match validate_host(&mut req) {
        Ok(host) => host,
        Err(reason) => {
            error!(target: "forward_proxy", "Invalid host: {reason}");
            return Ok(build_error_response(reason));
        }
    };
    let pool = H3ConnectionPool::global();
    // 创建 QUIC 连接
    let send_request = match create_quic_connection(pool.clone(), &host, resolvers).await {
        Ok(conn) => {
            debug!(target: "forward_proxy", "Quic connection established");
            conn
        }
        Err(error) => {
            let report = Report::from_error(error).to_string();
            error!(target: "forward_proxy", "Failed to create QUIC connection: {report}");
            return Ok(build_error_response(report));
        }
    };

    let request_upgrade = hyper::upgrade::on(&mut req);

    // 代理请求并返回响应
    match send(send_request, req).await {
        Ok(mut response) => {
            let response_upgrade = hyper::upgrade::on(&mut response);
            tokio::spawn(tunnel_upgrade(request_upgrade, response_upgrade).in_current_span());
            info!(target: "forward_proxy", "Request proxied successfully: {:?}", response);
            Ok(response)
        }
        Err(error) => {
            let reason = Report::from_error(io::Error::other(error)).to_string();
            error!(target: "forward_proxy", "Request proxy failed {reason}");
            Ok(build_error_response(reason))
        }
    }
}

/// 处理 CONNECT 隧道请求
pub async fn connect(
    req: Request<hyper::body::Incoming>,
    resolvers: Resolvers,
) -> Result<BoxResponse, hyper::Error> {
    tokio::spawn(async move {
        // 升级连接并处理后续请求
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                info!(target: "forward_proxy", "Establishing tunnel to the request uri");
                let service = service_fn(move |req| proxy(req, resolvers.clone()));
                if let Err(error) = http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(upgraded, service)
                    .await
                {
                    error!(target: "forward_proxy", "Connection handling failed: {}", Report::from_error(error));
                }
            }
            Err(error) => error!("Connection upgrade failed: {}", Report::from_error(error)),
        }
    });

    Ok(build_empty_response())
}

/// 将请求通过 quic 转发到目标服务器
async fn send(
    mut send_request: H3SendRequest,
    req: Request<hyper::body::Incoming>,
) -> Result<BoxResponse, Box<dyn std::error::Error + Send + Sync>> {
    let (parts, body) = req.into_parts();

    debug!(target: "forward_proxy", "Sending request");

    // 发送请求头
    let req = http::Request::from_parts(parts, ());
    let stream = send_request.send_request(req).await?;
    let (sender, mut recver) = stream.split();

    // 发送请求体
    tokio::spawn(
        async move {
            let mut body_stream = tokio_util::io::StreamReader::new(
                body.into_data_stream().map_err(std::io::Error::other),
            );
            let mut stream = H3Sink::new(sender);
            match tokio::io::copy(&mut body_stream, &mut stream).await {
                Ok(size) => debug!(target: "forward_proxy", "Request body sent: size={size}"),
                Err(error) => error!(target: "forward_proxy", "Error sending request body: {}", Report::from_error(error)),
            }
            match stream.shutdown().await {
                Ok(()) => info!(target: "forward_proxy", "Request finished sent"),
                Err(error) => error!(target: "forward_proxy", "Error sending request data end: {}",Report::from_error(error)),
            }
        }
        .in_current_span()
    );

    debug!(target: "forward_proxy", "Request body sent");

    // 接收响应头
    let (mut parts, _) = recver
        .recv_response()
        .await
        .inspect_err(|error| {
            error!(target: "forward_proxy", "Failed to receive response: {}", Report::from_error(error));
        })?
        .into_parts();
    debug!(target: "forward_proxy", "Received response headers: {parts:?}");
    parts.version = http::Version::HTTP_11;

    let recver_stream = H3Stream::new(recver);
    let body = BodyExt::boxed(StreamBody::new(recver_stream.map(|b| b.map(Frame::data))));
    debug!(target: "forward_proxy", "Response body receiver started");

    Ok(Response::from_parts(parts, body))
}

/// 创建 QUIC 连接并进行地址注册
async fn create_quic_connection(
    pool: Arc<H3ConnectionPool>,
    host: &str,
    resolvers: Resolvers,
) -> Result<H3SendRequest, Whatever> {
    let conn = pool
        .connect(host, resolvers)
        .await
        .whatever_context(format!("Connect to {host} failed"))?;
    let odcid = conn
        .quic
        .origin_dcid()
        .whatever_context("Get QUIC connection ODCID failed")?;
    tracing::Span::current().record("odcid", format!("{odcid:x}"));
    Ok(conn.h3.clone())
}

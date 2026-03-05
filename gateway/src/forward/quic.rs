use std::sync::Arc;

use h3x::gm_quic::H3Client;
use http::{Request, Response, uri::Authority};
use http_body_util::BodyExt;
use hyper::{server::conn::http1, service::service_fn};
use snafu::{Report, ResultExt, Whatever};
use tokio::io;
use tracing::{Instrument, error, info};

use super::BoxResponse;
use crate::forward::{build_empty_response, build_error_response, tunnel_upgrade, validate_host};

/// 处理普通 HTTP 请求
#[tracing::instrument(level = "info", skip_all, fields(odcid = tracing::field::Empty))]
pub async fn proxy(mut req: Request<hyper::body::Incoming>) -> Result<BoxResponse, hyper::Error> {
    // 验证主机合法性
    let host = match validate_host(&mut req) {
        Ok(host) => host,
        Err(reason) => {
            error!(target: "forward_proxy", "Invalid host: {reason}");
            return Ok(build_error_response(reason));
        }
    };
    let client = super::h3_client::global().await;
    // 创建 QUIC 连接
    let h3_conn = match connect(&client, &host).await {
        Ok(conn) => conn,
        Err(error) => {
            let report = Report::from_error(error).to_string();
            error!(target: "forward_proxy", "Failed to create QUIC connection: {report}");
            return Ok(build_error_response(report));
        }
    };

    let request_upgrade = hyper::upgrade::on(&mut req);

    // 代理请求并返回响应
    match send(h3_conn, req).await {
        Ok(mut response) => {
            let response_upgrade = hyper::upgrade::on(&mut response);
            tokio::spawn(tunnel_upgrade(request_upgrade, response_upgrade).in_current_span());
            info!(target: "forward_proxy", "Request proxied successfully: {:?}", response);
            Ok(response)
        }
        Err(error) => {
            let reason = Report::from_error(io::Error::other(error)).to_string();
            error!(target: "forward_proxy", "Forward request failed: {reason}");
            Ok(build_error_response(reason))
        }
    }
}

/// 处理 CONNECT 隧道请求
pub async fn connect_tunnel(
    req: Request<hyper::body::Incoming>,
) -> Result<BoxResponse, hyper::Error> {
    tokio::spawn(async move {
        // 升级连接并处理后续请求
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                info!(target: "forward_proxy", "Establishing tunnel to the request uri");
                let service = service_fn(proxy);
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
    h3_conn: Arc<h3x::connection::Connection<gm_quic::prelude::Connection>>,
    req: Request<hyper::body::Incoming>,
) -> Result<BoxResponse, Box<dyn std::error::Error + Send + Sync>> {
    // 使用 h3x 的 execute_hyper_request 一步完成：打开流、发送请求、接收响应
    let response = h3_conn.execute_hyper_request(req).await.map_err(|e| {
        error!(target: "forward_proxy", "execute_hyper_request failed: {e}");
        Box::new(e) as Box<dyn std::error::Error + Send + Sync>
    })?;

    // 将响应体转换为 BoxBody
    let (mut parts, body) = response.into_parts();
    parts.version = http::Version::HTTP_11;
    let body = BodyExt::boxed_unsync(body.map_err(io::Error::other));
    Ok(Response::from_parts(parts, body))
}

/// 通过 h3x 连接池获取连接
async fn connect(
    client: &H3Client,
    host: &str,
) -> Result<Arc<h3x::connection::Connection<gm_quic::prelude::Connection>>, Whatever> {
    let authority: Authority = host
        .parse()
        .whatever_context(format!("Invalid host: {host}"))?;
    let conn = client
        .connect(authority)
        .await
        .whatever_context(format!("Connect to {host} failed"))?;
    Ok(conn)
}

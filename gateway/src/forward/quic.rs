use std::{sync::Arc, time::Duration};

use futures::{StreamExt, TryStreamExt};
use http::{Request, Response};
use http_body_util::{BodyExt, StreamBody};
use hyper::{body::Frame, server::conn::http1, service::service_fn};
use qdns::Resolvers;
use tokio::{io::AsyncWriteExt, time::timeout};
use tracing::{Instrument, error, info};

use super::BoxResponse;
use crate::{
    forward::{build_empty_response, build_error_response, validate_host},
    h3::{H3SendRequest, H3Sink, H3Stream},
    pool::H3ConnectionPool,
};

pub async fn proxy(
    pool: Arc<H3ConnectionPool>,
    req: Request<hyper::body::Incoming>,
    resolvers: Resolvers,
) -> Result<BoxResponse, hyper::Error> {
    proxy_inner(pool, req, resolvers)
        .instrument(tracing::info_span!("proxy", odcid = tracing::field::Empty))
        .await
}

/// 处理普通 HTTP 请求
pub async fn proxy_inner(
    pool: Arc<H3ConnectionPool>,
    mut req: Request<hyper::body::Incoming>,
    resolvers: Resolvers,
) -> Result<BoxResponse, hyper::Error> {
    info!("[Forward] Request: {req:?}");

    let uri = req.uri().to_string();

    // 验证主机合法性
    let host = match validate_host(&mut req) {
        Ok(host) => host,
        Err(reason) => {
            error!("[Forward][{}] Invalid host: {}", uri, reason);
            return Ok(build_error_response(reason));
        }
    };

    // 创建 QUIC 连接
    let send_request = match create_quic_connection(pool, &host, resolvers).await {
        Ok(conn) => conn,
        Err(msg) => {
            error!(
                "[Forward][{}] Failed to create QUIC connection: {}",
                uri, msg
            );
            return Ok(build_error_response(msg));
        }
    };

    info!("[Forward][{}]: quic connection established", uri);

    // 代理请求并返回响应
    match send(send_request, req).await {
        Ok(response) => {
            info!(
                "[Forward][{}] Request proxied successfully: {:?}",
                uri, response
            );
            Ok(response)
        }
        Err(err) => {
            let reason = format!("[Forward][{uri}] Request proxy failed: {err}");
            error!("{}", reason);
            Ok(build_error_response(reason))
        }
    }
}

/// 处理 CONNECT 隧道请求
pub async fn connect(
    pool: Arc<H3ConnectionPool>,
    req: Request<hyper::body::Incoming>,
    resolvers: Resolvers,
) -> Result<BoxResponse, hyper::Error> {
    let uri = req.uri().to_string();

    info!("[CONNECT] Establishing tunnel to {}", uri);

    // 升级连接并处理后续请求
    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                info!("[CONNECT]: tunnel established to {}", uri);
                let service = service_fn(move |req| proxy(pool.clone(), req, resolvers.clone()));
                if let Err(err) = http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(upgraded, service)
                    .await
                {
                    error!("[CONNECT][{uri}] Connection handling failed: {err:?}");
                }
            }
            Err(err) => error!("Connection upgrade failed {uri}: {err:?}"),
        }
    });

    Ok(build_empty_response())
}

/// 将请求通过 quic 转发到目标服务器
async fn send(
    mut send_request: H3SendRequest,
    req: Request<hyper::body::Incoming>,
) -> Result<BoxResponse, Box<dyn std::error::Error + Send + Sync>> {
    let uri = req.uri().to_string();
    let (parts, body) = req.into_parts();

    info!("[Proxy][{}] Sending request", uri);

    // 发送请求头
    let req = http::Request::from_parts(parts, ());
    let stream = send_request.send_request(req).await?;
    let (sender, mut recver) = stream.split();

    // 发送请求体
    tokio::spawn({
        let uri = uri.clone();
        async move {
            let mut body_stream = tokio_util::io::StreamReader::new(
                body.into_data_stream().map_err(std::io::Error::other),
            );
            let mut stream = H3Sink::new(sender);
            match tokio::io::copy(&mut body_stream, &mut stream).await {
                Ok(size) => info!("[Proxy][{uri}] Request body sent: size={size}"),
                Err(e) => error!("[Proxy][{uri}] Error sending request body: {e}"),
            }
            match stream.shutdown().await {
                Ok(()) => info!("[Proxy][{}] Request finished sent", uri),
                Err(e) => error!("[Proxy][{}] Error sending request data end: {}", uri, e),
            }
        }
        .in_current_span()
    });

    info!("[Forward][{}] Request body sent", uri);

    // 接收响应头
    let (mut parts, _) = recver
        .recv_response()
        .await
        .inspect_err(|e| {
            error!("[Forward][{}] Failed to receive response: {}", uri, e);
        })?
        .into_parts();
    info!("[Forward] Received response headers: {:?}", parts);
    parts.version = http::Version::HTTP_11;

    let recver_stream = H3Stream::new(recver);
    let body = BodyExt::boxed(StreamBody::new(recver_stream.map(|b| b.map(Frame::data))));
    info!("[Forward] Response body receiver started");

    Ok(Response::from_parts(parts, body))
}

/// 创建 QUIC 连接并进行地址注册
async fn create_quic_connection(
    pool: Arc<H3ConnectionPool>,
    host: &str,
    resolvers: Resolvers,
) -> Result<H3SendRequest, String> {
    let mut ns_resolver = resolvers.lookup(host);
    let endpoints = tokio_stream::StreamExt::next(&mut ns_resolver).await;
    let (_, remote_endpoints) = match endpoints {
        Some(endpoints) => endpoints,
        None => {
            return Err(format!("Failed to resolve host: {host}"));
        }
    };

    match timeout(
        Duration::from_millis(1000),
        pool.connect(host, remote_endpoints),
    )
    .await
    {
        Ok(result) => match result {
            Ok(conn) => {
                tokio::spawn({
                    let conn = conn.clone();
                    async move {
                        // TODO: 在这里添加地址可能有点晚了，应该在 quic client 创建之后马上添加
                        while let Some((_, remote_endpoints)) =
                            tokio_stream::StreamExt::next(&mut ns_resolver).await
                        {
                            remote_endpoints
                                .into_iter()
                                .map(|ep| ep.into())
                                .for_each(|addr| {
                                    _ = conn.quic.add_peer_endpoint(addr);
                                });
                        }
                    }
                });
                let origin_dcid = match conn.quic.origin_dcid() {
                    Ok(dcid) => dcid,
                    Err(e) => {
                        error!("Failed to get origin DCID: {}", e);
                        return Err("Failed to get origin DCID".to_string());
                    }
                };
                tracing::Span::current().record("odcid", format!("{origin_dcid:x}"));
                Ok(conn.h3.clone())
            }
            Err(e) => Err(format!("Failed to connect to host: {e}")),
        },
        Err(e) => Err(format!("Timeout to connect to host: {e}")),
    }
}

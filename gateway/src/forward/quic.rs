use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::Buf;
use gm_quic::QuicClient;
use http::{Request, Response};
use http_body_util::{BodyExt, StreamBody};
use hyper::{body::Frame, server::conn::http1, service::service_fn};
use tokio::time::timeout;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tracing::{Instrument, error, info, warn};

use super::{BoxResponse, H3Conn, H3SendRequest};
use crate::{
    Resolver,
    dns::UdpResolver,
    forward::{CHANNEL_BUFFER_SIZE, build_empty_response, build_error_response, validate_host},
};

const MAX_RETRY_COUNT: usize = 3; // QUIC 连接最大重试次数

pub async fn proxy(
    quic_client: Arc<QuicClient>,
    req: Request<hyper::body::Incoming>,
    resolver: SocketAddr,
) -> Result<BoxResponse, hyper::Error> {
    proxy_inner(quic_client, req, resolver)
        .instrument(tracing::info_span!("proxy", odcid = tracing::field::Empty))
        .await
}

/// 处理普通 HTTP 请求
pub async fn proxy_inner(
    quic_client: Arc<QuicClient>,
    req: Request<hyper::body::Incoming>,
    resolver: SocketAddr,
) -> Result<BoxResponse, hyper::Error> {
    let uri = req.uri().to_string();
    info!("[Forward] Request: {}", uri);

    // 验证主机合法性
    let host = match validate_host(&req) {
        Ok(host) => host,
        Err(reason) => {
            error!("[Forward][{}] Invalid host: {}", uri, reason);
            return Ok(build_error_response(reason));
        }
    };

    // 创建 QUIC 连接
    let (mut _h3_conn, send_request) =
        match create_quic_connection(quic_client, host, resolver).await {
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
            let reason = format!("[Forward][{}] Request proxy failed: {}", uri, err);
            error!("{}", reason);
            Ok(build_error_response(reason))
        }
    }
}

/// 处理 CONNECT 隧道请求
pub async fn connect(
    quic_client: Arc<QuicClient>,
    req: Request<hyper::body::Incoming>,
    resolver: SocketAddr,
) -> Result<BoxResponse, hyper::Error> {
    let uri = req.uri().to_string();

    // 验证主机合法性
    let _host = match validate_host(&req) {
        Ok(host) => host,
        Err(reason) => return Ok(build_error_response(reason)),
    };

    info!("[CONNECT] Establishing tunnel to {}", uri);

    // 升级连接并处理后续请求
    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                info!("[CONNECT]: tunnel established to {}", uri);
                let service = service_fn(move |req| proxy(quic_client.clone(), req, resolver));
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
    let (mut sender, mut recver) = stream.split();

    // 发送请求体
    tokio::spawn({
        let uri = uri.clone();
        async move {
            let mut body_stream = body.into_data_stream();
            while let Some(Ok(chunk)) = body_stream.next().await {
                if let Err(e) = sender.send_data(chunk).await {
                    error!("Error sending request data: {}", e);
                    break;
                }
            }

            // TODO 没发 fin
            match sender.finish().await {
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

    // 准备响应体通道
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_BUFFER_SIZE);
    let body = StreamBody::new(ReceiverStream::new(rx));

    // 异步接收响应体数据
    tokio::spawn(
        async move {
            let _send_request = send_request.clone();
            loop {
                match recver.recv_data().await {
                    Ok(Some(mut buf)) => {
                        let bytes = buf.copy_to_bytes(buf.remaining());
                        if let Err(e) = tx.send(Ok(Frame::data(bytes))).await {
                            error!("[Proxy][{}] Failed to send response: {}", uri, e);
                            break;
                        }
                    }
                    Ok(None) => {
                        info!("[Proxy][{}] Response received completely", uri);
                        break;
                    }
                    Err(err) => {
                        info!("[Proxy][{}] Data receiving error: {}", uri, err);
                        break;
                    }
                }
            }
        }
        .in_current_span(),
    );

    info!("[Forward] Response body receiver started");

    Ok(Response::from_parts(parts, body))
}

/// 创建 QUIC 连接并进行地址注册
async fn create_quic_connection(
    quic_client: Arc<QuicClient>,
    host: &str,
    resolver: SocketAddr,
) -> Result<(H3Conn, H3SendRequest), String> {
    let resolver = UdpResolver::new(resolver);

    let mut remote_endpoints = Vec::new();

    // DNS 解析重试
    for retry in 0..MAX_RETRY_COUNT {
        match resolver.look_up(host).await {
            Ok(endpoints) => {
                info!("[DNS]: resolved: {host} -> {endpoints:?}");
                remote_endpoints = endpoints;
                break;
            }
            Err(err) => {
                warn!("[DNS] lookup {host} failed: {err} retry: {retry}");
                continue;
            }
        }
    }

    if remote_endpoints.is_empty() {
        return Err(format!("[DNS] lookup failed for: {host}"));
    }

    for (index, endpoint) in remote_endpoints.iter().enumerate() {
        // 建立 QUIC 连接
        let conn = match quic_client.connect(host, *endpoint) {
            Ok(conn) => conn,
            Err(e) => {
                warn!(
                    "[Forward] Failed to connect to {}: {}, retry: {} of {}",
                    endpoint, e, index, endpoint
                );
                continue;
            }
        };

        // HTTP/3 客户端
        let gm_quic_conn = h3_shim::QuicConnection::new(conn.clone()).await;

        // 创建 H3 客户端并设置超时
        match timeout(
            Duration::from_millis(5000 * (index + 1) as u64),
            h3::client::new(gm_quic_conn),
        )
        .await
        {
            Ok(result) => {
                let origin_dcid = match conn.origin_dcid() {
                    Ok(dcid) => dcid,
                    Err(e) => {
                        error!("Failed to get origin DCID: {}", e);
                        continue;
                    }
                };

                return {
                    tracing::Span::current().record("odcid", format!("{:x}", origin_dcid));
                    result.map_err(|e| format!("h3 client creation failed: {}", e))
                };
            }
            Err(e) => {
                error!(
                    "[Forward] H3 client creation failed: {}, retry: {} of {}",
                    e, index, endpoint
                );
            }
        }
    }

    Err("Maximum retry attempts exceeded".to_string())
}

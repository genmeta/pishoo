use http::Request;
use http_body_util::{BodyExt, combinators::UnsyncBoxBody};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use snafu::{Report, ResultExt};
use tokio::net::TcpStream;
use tracing::{Instrument, debug, error, info};

use super::BoxResponse;
use crate::{
    error::{BoxError, Whatever},
    forward::{ForwardRequestError, build_empty_response, build_error_response, tunnel_upgrade},
};

/// 普通代理 HTTP 请求
pub async fn proxy(mut req: Request<hyper::body::Incoming>) -> Result<BoxResponse, hyper::Error> {
    let original_uri = req.uri().clone();

    let host = match original_uri.host() {
        Some(host) => host,
        None => {
            let error = ForwardRequestError::MissingHostInUri;
            error!(error = %Report::from_error(&error), "missing host in uri");
            return Ok(build_error_response(Report::from_error(&error).to_string()));
        }
    };

    let port = original_uri.port_u16().unwrap_or(80);

    let stream = match TcpStream::connect((host, port))
        .await
        .whatever_context::<_, Whatever>(format!("failed to connect to upstream {host}:{port}"))
    {
        Ok(stream) => stream,
        Err(error) => {
            error!(error = %Report::from_error(&error), %host, port, "connect to upstream failed");
            return Ok(build_error_response(Report::from_error(&error).to_string()));
        }
    };

    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(TokioIo::new(stream))
        .await?;

    // Terminates when the HTTP/1.1 connection closes or encounters an error.
    tokio::spawn(
        async move {
            // 启用连接升级支持,用于处理 WebSocket
            if let Err(error) = conn.with_upgrades().await {
                error!(error = %Report::from_error(&error), "connection failed");
            }
        }
        .in_current_span(),
    );

    // 将绝对 URI 转换为相对路径,避免目标服务器解析错误
    // 例如: http://localhost:5173/@vite/client -> /@vite/client
    //
    // FIX ME: QUIC代理是否也需要这样去做？
    let path_and_query = original_uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let relative_uri = http::Uri::builder()
        .path_and_query(path_and_query)
        .build()
        .unwrap_or_else(|_| http::Uri::from_static("/"));

    *req.uri_mut() = relative_uri;

    debug!(%host, port, path_and_query, "forwarding request");

    let request_upgrade = hyper::upgrade::on(&mut req);
    let mut resp = sender.send_request(req).await?;
    let response_upgrade = hyper::upgrade::on(&mut resp);

    // Terminates when either end of the tunnel closes the connection.
    tokio::spawn(tunnel_upgrade(request_upgrade, response_upgrade).in_current_span());

    Ok(resp.map(|b| UnsyncBoxBody::new(b.map_err(BoxError::from))))
}

pub async fn connect(req: Request<hyper::body::Incoming>) -> Result<BoxResponse, hyper::Error> {
    let Some(addr) = req.uri().authority().map(|auth| auth.to_string()) else {
        let error = ForwardRequestError::MissingConnectAuthority;
        error!(error = %Report::from_error(&error), "missing host in connect uri");
        let mut resp = build_error_response(Report::from_error(&error).to_string());
        *resp.status_mut() = http::StatusCode::BAD_REQUEST;
        return Ok(resp);
    };
    // 升级连接并处理后续请求
    tokio::task::spawn(
        async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    if let Err(error) = tunnel(upgraded, addr).await {
                        error!(error = %Report::from_error(&error), "connect proxy aborted");
                    }
                }
                Err(error) => {
                    error!(error = %Report::from_error(&error), "connection upgrade failed")
                }
            }
        }
        .in_current_span(),
    );

    Ok(build_empty_response())
}

/// 代理 CONNECT 后的 HTTP 请求
#[tracing::instrument(level = "info", skip_all, fields(%addr))]
async fn tunnel(upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    // Print message when done
    info!(from_client, from_server, "connect tunnel completed");

    Ok(())
}

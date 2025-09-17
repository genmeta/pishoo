use std::io;

use http::{Method, Request, Response};
use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use snafu::Report;
use tokio::net::TcpStream;
use tracing::{debug, error, info};

use super::BoxResponse;
use crate::forward::{build_empty_response, build_error_response};

/// 普通代理 HTTP 请求
pub async fn proxy(req: Request<hyper::body::Incoming>) -> Result<BoxResponse, hyper::Error> {
    debug!(target: "forward_proxy", request=?req);

    if req.method() == Method::CONNECT {
        let Some(addr) = req.uri().authority().map(|auth| auth.to_string()) else {
            error!(target: "forward_proxy", "Missing host in uri");
            let mut resp = build_error_response("CONNECT must be to a valid host".to_string());
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            return Ok(resp);
        };

        tokio::task::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    if let Err(e) = tunnel(upgraded, addr).await {
                        error!(target: "forward_proxy", "Server io error: {}", Report::from_error(e));
                    };
                }
                Err(e) => {
                    error!(target: "forward_proxy", "Upgrade error: {}", Report::from_error(e))
                }
            }
        });

        Ok(build_empty_response())
    } else {
        let host = match req.uri().host() {
            Some(host) => host,
            None => {
                error!(target: "forward_proxy", "Missing host in uri");
                return Ok(build_error_response("Missing host in uri".to_string()));
            }
        };

        let port = req.uri().port_u16().unwrap_or(80);

        let stream = match TcpStream::connect((host, port)).await {
            Ok(stream) => stream,
            Err(error) => {
                error!(target: "forward_proxy", "Connect to {host}:{port} error {}", Report::from_error(&error));
                return Ok(build_error_response(error.to_string()));
            }
        };

        let io = TokioIo::new(stream);

        let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;
        tokio::task::spawn(async move {
            if let Err(error) = conn.await {
                error!(target: "forward_proxy", "Connection failed: {}", Report::from_error(error));
            }
        });

        let resp = sender.send_request(req).await?;
        let (parts, body) = resp.into_parts();
        let resp = Response::from_parts(parts, BoxBody::new(body.map_err(io::Error::other)));
        Ok(resp)
    }
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
    info!(
        target: "forward_proxy",
        "client wrote {from_client} bytes and received {from_server} bytes",
    );

    Ok(())
}

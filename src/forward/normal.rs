use bytes::Bytes;
use http::{Method, Request, Response};
use http_body_util::{BodyExt, StreamBody};
use hyper::{body::Frame, upgrade::Upgraded};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tracing::{error, info};

use super::BoxResponse;
use crate::forward::{build_empty_response, build_error_response};

/// 普通代理 HTTP 请求
pub async fn proxy(req: Request<hyper::body::Incoming>) -> Result<BoxResponse, hyper::Error> {
    info!("[normal_proxy] req: {:?}", req);

    if Method::CONNECT == req.method() {
        if let Some(addr) = req.uri().authority().map(|auth| auth.to_string()) {
            tokio::task::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        if let Err(e) = tunnel(upgraded, addr).await {
                            error!("server io error: {}", e);
                        };
                    }
                    Err(e) => error!("upgrade error: {}", e),
                }
            });

            Ok(build_empty_response())
        } else {
            error!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = build_error_response("CONNECT must be to a socket address".to_string());
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            Ok(resp)
        }
    } else {
        let host = match req.uri().host() {
            Some(host) => host,
            None => {
                error!("no host in uri: {:?}", req.uri());
                return Ok(build_error_response("no host in uri".to_string()));
            }
        };

        let port = req.uri().port_u16().unwrap_or(80);

        let stream = match TcpStream::connect((host, port)).await {
            Ok(stream) => stream,
            Err(e) => {
                error!("connect error: {}", e);
                return Ok(build_error_response(e.to_string()));
            }
        };

        let io = TokioIo::new(stream);

        let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                info!("Connection failed: {:?}", err);
            }
        });

        let resp = sender.send_request(req).await?;
        let (parts, body) = resp.into_parts();
        let mut data_stream = body.into_data_stream();

        let (tx, rx) =
            tokio::sync::mpsc::channel::<std::result::Result<Frame<Bytes>, hyper::Error>>(128);
        let body_stream = StreamBody::new(ReceiverStream::new(rx));

        tokio::spawn(async move {
            while let Some(Ok(chunk)) = data_stream.next().await {
                _ = tx.send(Ok(Frame::data(chunk))).await.inspect_err(|e| {
                    error!("Error sending data frame: {:?}", e);
                });
            }
        });

        let resp = Response::from_parts(parts, body_stream);
        Ok(resp)
    }
}

/// 代理 CONNECT 后的 HTTP 请求
async fn tunnel(upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    // Print message when done
    info!(
        "client wrote {} bytes and received {} bytes",
        from_client, from_server
    );

    Ok(())
}

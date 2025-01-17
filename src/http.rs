use std::{collections::HashMap, net::SocketAddr};

use http::StatusCode;
use hyper::service::service_fn;
use tokio::net::TcpListener;
use tracing::error;

use crate::error::Result;
use crate::{
    handle::handle_http,
    parse::{router::Router, server::Server},
};

pub mod http3;

pub async fn serve(addr: SocketAddr, servers: HashMap<String, Server>) -> Result<()> {
    let routers = servers
        .into_iter()
        .map(|(name, server)| (name, server.router))
        .collect();

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(io, service_fn(proxy))
                .with_upgrades()
                .await
            {
                println!("Failed to serve connection: {:?}", err);
            }
        });
    }

    Ok(())
}

#[axum::debug_handler]
pub async fn handle_request(
    State(routers): State<HashMap<String, Router>>,
    req: Request<Body>,
) -> Response {
    match handle_http(routers, req).await {
        Ok(res) => res,
        Err(err) => {
            error!("{}", err);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap()
        }
    }
}

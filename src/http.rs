use std::{collections::HashMap, net::SocketAddr};

use axum::{
    body::Body,
    extract::{Request, State},
    response::Response,
    routing::any,
};
use http::StatusCode;
use tracing::error;

use crate::{
    handle::handle_http,
    parse::{router::Router, server::Server},
};

pub mod http3;

pub async fn serve(bind: SocketAddr, servers: HashMap<String, Server>) {
    let routers = servers
        .into_iter()
        .map(|(name, server)| (name, server.router))
        .collect();

    let app = axum::Router::new()
        .route("/", any(handle_request))
        .with_state(routers);

    let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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

//! Test driver using real h3x streams for both sides of an exchange.
#![allow(dead_code)]
#[path = "h3.rs"]
mod h3;

use h3x::{ReadRequest, ReadResponse, WriteRequest};
use pishoo::{
    outgoing::OutgoingContext,
    sandbox::Sandbox,
    wasm::{Error, Limits},
};
use tokio::{io::AsyncWriteExt, task::JoinHandle};

pub struct Exchange {
    pub server: JoinHandle<Result<(), Error>>,
    response: JoinHandle<h3x::Result<h3x::Response<h3x::R>>>,
    upload: JoinHandle<h3x::Result<()>>,
}
impl Exchange {
    pub async fn response(&mut self) -> h3x::Result<h3x::Response<h3x::R>> {
        (&mut self.response).await.unwrap()
    }
    pub async fn result(&mut self) -> Result<(), Error> {
        (&mut self.server).await.unwrap()
    }
}
impl Drop for Exchange {
    fn drop(&mut self) {
        self.server.abort();
        self.response.abort();
        self.upload.abort();
    }
}

pub fn request(path: &str) -> http::Request<h3x::ArcWndBuf> {
    http::Request::post(format!("https://example.com{path}"))
        .body(h3x::ArcWndBuf::new(8192))
        .unwrap()
}

pub async fn empty_request(path: &str) -> http::Request<h3x::ArcWndBuf> {
    let mut request = request(path);
    request.body_mut().shutdown().await.unwrap();
    request
}

pub async fn start(
    sandbox: &Sandbox,
    mut request: http::Request<h3x::ArcWndBuf>,
    limits: Limits,
) -> Exchange {
    let (client, server) = h3::connection_pair();
    // Simulate authenticated host middleware: extensions are injected at the
    // receiver and never transmitted as client-controlled HTTP headers.
    let context = request.extensions_mut().remove::<OutgoingContext>();
    let method = request.method().clone();
    let (ws, rs) = client.open_bi().await.unwrap();
    let qpack = client.qpack().clone();
    let upload = tokio::spawn(async move { ws.write_request(request.into(), qpack).await });
    let response =
        tokio::spawn(async move { rs.read_response(method, client.qpack().clone()).await });
    let (ws, rs) = server.accept_bi().await.unwrap();
    let incoming = rs.read_request(server.qpack().clone()).await.unwrap();
    let (mut parts, body) = incoming.into_parts();
    if let Some(context) = context {
        parts.extensions.insert(context);
    }
    let incoming = h3x::Request::from_parts(parts, body);
    let sandbox = sandbox.clone();
    let server = tokio::spawn(async move {
        sandbox
            .handle_stream(ws, incoming, server.qpack().clone(), limits)
            .await
    });
    Exchange {
        server,
        response,
        upload,
    }
}

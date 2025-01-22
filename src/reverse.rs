use std::{net::SocketAddr, sync::Arc};

use bytes::{Buf, Bytes};
use futures::future;
use h3_shim::{QuicClient, qbase::param::ClientParameters};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn};
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use crate::{
    parse::{router::Router, server::ReverseConfig},
    support::TokioIo,
};

#[derive(Clone)]
pub struct ReverseServer;

static ALPN: &[u8] = b"h3";

impl ReverseServer {
    pub async fn serve(addr: SocketAddr, server: ReverseConfig) {
        let listener = TcpListener::bind(addr).await.expect("bind tcp listener");
        info!("Listening on http://{}", addr);

        let router = Arc::new(server.router);

        while let Ok((stream, _)) = listener.accept().await {
            let io = TokioIo::new(stream);
            tokio::task::spawn({
                let router = router.clone();
                async move {
                    if let Err(err) = http1::Builder::new()
                        .preserve_header_case(true)
                        .title_case_headers(true)
                        .serve_connection(io, service_fn(|req| handler(router.clone(), req)))
                        .with_upgrades()
                        .await
                    {
                        println!("Failed to serve connection: {:?}", err);
                    }
                }
            });
        }
        error!("server error address: {addr}");
    }
}

async fn handler(
    _router: Arc<Router>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    debug!("req: {:?}", req);

    // TODO 根据 router location 匹配请求

    let host = req
        .uri()
        .authority()
        .map(|auth| auth.host().to_string())
        .expect("host not found in request uri");

    debug!("proxy uri host: {}", host);

    // TODO DNS 解析
    let addr = "127.0.0.1:6001";
    info!("dns resolved: {} -> {}", req.uri(), addr);

    // 创建 QUIC 客户端
    let quic_client = create_quic_client().await;

    // 连接到远程服务器
    let conn = quic_client
        .connect(host, addr.parse().unwrap())
        .expect("connect quic client");

    // 创建 H3 客户端
    let (conn, send_request) = create_h3_client(conn).await;

    // 并发执行驱动和请求
    let (derive, request) = tokio::join!(
        tokio::spawn(run_quic_driver(conn)),
        tokio::spawn(handle_request(send_request, req))
    );

    // 处理结果
    derive.expect("run quic driver").expect("quic driver");
    let response = request.expect("handle request").expect("response");

    debug!("response: {:#?}", response);
    Ok(response)
}

/// 创建 QUIC 客户端
async fn create_quic_client() -> QuicClient {
    let mut params = ClientParameters::default();
    params.set_initial_max_streams_bidi(100u32.into());
    params.set_initial_max_streams_uni(100u32.into());
    params.set_initial_max_data((1u32 << 20).into());
    params.set_initial_max_stream_data_uni((1u32 << 20).into());
    params.set_initial_max_stream_data_bidi_local((1u32 << 20).into());
    params.set_initial_max_stream_data_bidi_remote((1u32 << 20).into());

    QuicClient::builder()
        .with_root_certificates(crate::common::root_cert())
        .without_cert()
        .with_alpns([ALPN.into()])
        .with_parameters(params)
        .bind("127.0.0.1:0")
        .expect("bind quic client")
        .build()
}

/// 创建 H3 客户端
async fn create_h3_client(
    conn: Arc<gm_quic::QuicConnection>,
) -> (
    h3::client::Connection<h3_shim::QuicConnection, Bytes>,
    h3::client::SendRequest<h3_shim::OpenStreams, Bytes>,
) {
    let gm_quic_conn = h3_shim::QuicConnection::new(conn).await;
    let (conn, send_request) = h3::client::new(gm_quic_conn)
        .await
        .expect("create h3 client");

    (conn, send_request)
}

/// 运行 QUIC 驱动
async fn run_quic_driver(
    mut conn: h3::client::Connection<h3_shim::QuicConnection, Bytes>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    future::poll_fn(|cx| conn.poll_close(cx)).await?;
    Ok(())
}

/// 处理 HTTP 请求
async fn handle_request(
    mut send_request: h3::client::SendRequest<h3_shim::OpenStreams, Bytes>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn std::error::Error + Send + Sync>> {
    info!("sending request ...");

    let (parts, body) = req.into_parts();
    let bytes = body.collect().await?.to_bytes();

    let req = http::Request::from_parts(parts, ());
    let mut stream = send_request.send_request(req).await?;
    stream.send_data(bytes).await?;
    stream.finish().await?;

    info!("receiving response ...");
    let (mut parts, _) = stream.recv_response().await?.into_parts();
    info!("response: {:?} {}", parts.version, parts.status);
    info!("headers: {:#?}", parts.headers);

    let mut body = Vec::new();
    while let Some(chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.chunk());
    }
    parts.version = http::Version::HTTP_11;

    Ok(Response::from_parts(parts, full(Bytes::from(body))))
}

pub fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

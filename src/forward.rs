use std::{
    net::SocketAddr,
    sync::{Arc, atomic::AtomicUsize},
};

use bytes::{Buf, Bytes};
use futures::FutureExt;
use gm_quic::{ClientParameters, Pathway, QuicInterface, Socket, prelude::Endpoint};
use h3_shim::QuicClient;
use http::StatusCode;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn};
use qconnection::traversal::NatType;
use qinterface::handy::Usc;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::{
    dns::{AGENT, DNS_SERVER, get_or_create_addr_rigistery, resolve_dns},
    parse::server::ForwardConfig,
    support::TokioIo,
};

#[derive(Debug, Clone, Copy)]
pub struct LocalHost {
    endpoint: Endpoint,
    socket: Socket,
    registry_bind: SocketAddr,
    outer: SocketAddr,
    nat_type: NatType,
}

pub struct ForwardServer;

static ALPN: &[u8] = b"h3";

impl ForwardServer {
    pub async fn serve(addr: SocketAddr, _server: ForwardConfig) {
        let listener = TcpListener::bind(addr).await.expect("bind tcp listener");
        info!("Listening on http://{}", addr);

        let (quic_client, local_host) = bind_registry(addr).await;

        while let Ok((stream, _)) = listener.accept().await {
            let io = TokioIo::new(stream);
            tokio::task::spawn({
                let quic_client = quic_client.clone();
                async move {
                    let serve = service_fn(|req| {
                        if req.method() == "CONNECT" {
                            handler_connect(quic_client.clone(), local_host, req).boxed()
                        } else {
                            handler(quic_client.clone(), local_host, req).boxed()
                        }
                    });
                    let result = http1::Builder::new()
                        .preserve_header_case(true)
                        .title_case_headers(true)
                        .serve_connection(io, serve)
                        .with_upgrades()
                        .await;
                    if let Err(err) = result {
                        println!("Failed to serve connection: {:?}", err);
                    }
                }
            });
        }
        error!("server error address: {addr}");
    }
}

async fn handler_connect(
    quic_client: Arc<QuicClient>,
    local_host: LocalHost,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let uri = req.uri().to_string();
    info!("CONNECT request to {}", uri);

    tokio::task::spawn({
        async move {
            let upgraded = if let Ok(upgraded) = hyper::upgrade::on(req).await {
                TokioIo::new(upgraded)
            } else {
                error!("Failed to upgrade connection {uri}");
                return;
            };
            http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(
                    upgraded.inner(),
                    service_fn(|req| handler(quic_client.clone(), local_host, req)),
                )
                .await
                .inspect_err(|err| error!("Failed to serve connection: {err}"))
                .expect("serve connection");
            info!("CONNECT tunnel established to {}", uri);
        }
    });

    Ok(Response::new(empty()))
}

async fn bind_registry(bind: SocketAddr) -> (Arc<QuicClient>, LocalHost) {
    let addr_registry = get_or_create_addr_rigistery(bind).unwrap();
    let outer = addr_registry
        .outer_addr()
        .await
        .expect("fail to get outer addr");
    let nat_type = addr_registry
        .nat_type()
        .await
        .expect("fail to get nat type");

    info!("outer addr {}", outer);
    info!("nat type {:?}", nat_type);
    let registry_bind = addr_registry.bind_addr();
    let usc = Arc::new(Usc::new(addr_registry.iface()).expect("create usc"));
    info!("bind addr: {}", registry_bind);

    let quic_client = create_quic_client(registry_bind, usc).await;
    let agent: SocketAddr = AGENT;
    let local_endpoint = Endpoint::Relay { agent, outer };
    let socket = Socket::new(registry_bind, agent);

    let local_host = LocalHost {
        endpoint: local_endpoint,
        socket,
        registry_bind,
        outer,
        nat_type,
    };

    (Arc::new(quic_client), local_host)
}

async fn create_quic_conn(
    quic_client: Arc<QuicClient>,
    local_host: LocalHost,
    host: &str,
) -> (
    h3::client::Connection<h3_shim::QuicConnection, Bytes>,
    h3::client::SendRequest<h3_shim::OpenStreams, Bytes>,
) {
    // TODO 解析失败场景

    // DNS 解析
    let remote = resolve_dns(host, DNS_SERVER.parse().expect("parse dns server"))
        .await
        .expect("resolve dns");

    info!("dns resolved: {} -> {:?}", host, remote);

    let pathway = Pathway::new(local_host.endpoint, remote);
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    // 建立 QUIC 连接
    let conn = tracing::trace_span!(
        "quic",
        seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
    .in_scope(|| {
        quic_client
            .connect(host, local_host.socket, pathway)
            .expect("connect quic client")
    });

    let _ = conn.add_address(
        local_host.registry_bind,
        local_host.outer,
        1,
        local_host.nat_type,
    );

    // 创建 HTTP/3 客户端
    let gm_quic_conn = h3_shim::QuicConnection::new(conn).await;
    let (h3_conn, h3_sender) = h3::client::new(gm_quic_conn)
        .await
        .expect("create h3 client");

    (h3_conn, h3_sender)
}

// TODO:  serve connection: hyper::Error(IncompleteMessage)
async fn handler(
    quic_client: Arc<QuicClient>,
    local_host: LocalHost,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    info!("request: {:?}", req);
    let uri = req.uri().to_string();

    let not_found = Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(full(Bytes::new()))
        .expect("static response should be valid");

    // 获取请求主机头
    let host = match req.uri().authority().map(|auth| auth.host()) {
        Some(host) => host.to_string(),
        None => match req.headers().get("host") {
            Some(host) => host.to_str().unwrap().to_string(),
            None => return Ok(not_found),
        },
    };

    info!("[fff][{}]: prepare to create quic conn", uri);
    let (_h3_conn, h3_request) = create_quic_conn(quic_client, local_host, &host).await;
    info!("[fff][{}]: created quic conn", uri);
    let response = proxy_request(h3_request, req)
        .await
        .expect("proxy request error");
    Ok(response)
}

/// 创建 QUIC 客户端
async fn create_quic_client(bind: SocketAddr, usc: Arc<Usc>) -> QuicClient {
    let mut params = ClientParameters::default();
    params.set_initial_max_streams_bidi(100u32.into());
    params.set_initial_max_streams_uni(100u32.into());
    params.set_initial_max_data((1u32 << 20).into());
    params.set_initial_max_stream_data_uni((1u32 << 20).into());
    params.set_initial_max_stream_data_bidi_local((1u32 << 20).into());
    params.set_initial_max_stream_data_bidi_remote((1u32 << 20).into());

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(crate::common::root_cert())
        .with_no_client_auth();

    tls_config.alpn_protocols = vec![ALPN.into()];
    tls_config.resumption = rustls::client::Resumption::disabled();
    tls_config.key_log = Arc::new(rustls::KeyLogFile::new());

    QuicClient::builder_with_tls(tls_config)
        .with_parameters(params)
        .reuse_interfaces()
        .with_iface_binder(move |addr| {
            if addr == usc.local_addr()? {
                Ok(usc.clone())
            } else {
                Ok(Arc::new(Usc::bind(addr)?))
            }
        })
        .bind(bind)
        .expect("bind quic client")
        .build()
}

/// 处理 HTTP 请求
async fn proxy_request(
    mut sender: h3::client::SendRequest<h3_shim::OpenStreams, Bytes>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn std::error::Error + Send + Sync>> {
    let (parts, body) = req.into_parts();
    let body = body.collect().await?.to_bytes();

    let uri = parts.uri.to_string();
    info!("sending request: {}", uri);

    let req = http::Request::from_parts(parts, ());
    let mut stream = sender.send_request(req).await?;
    stream.send_data(body).await?;
    stream.finish().await?;

    let (mut parts, _) = stream.recv_response().await?.into_parts();

    let mut body = Vec::new();
    info!("[{}]: receiving response body", uri);
    let mut sum_bytes = 0;
    while let Some(chunk) = stream.recv_data().await? {
        sum_bytes += chunk.chunk().len();
        info!(
            "[{}]: received response chunk: {} , sum_bytes: {}",
            uri,
            chunk.chunk().len(),
            sum_bytes,
        );
        body.extend_from_slice(chunk.chunk());
    }
    info!("[{}]: received response body", uri);
    parts.version = http::Version::HTTP_11;
    Ok(Response::from_parts(parts, full(Bytes::from(body))))
}

pub fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

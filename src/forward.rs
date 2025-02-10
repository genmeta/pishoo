use std::{net::SocketAddr, sync::Arc};

use bytes::{Buf, Bytes};
use futures::{FutureExt, future};
use gm_quic::{ClientParameters, Pathway, QuicInterface, Socket, prelude::Endpoint};
use h3_shim::QuicClient;
use http::StatusCode;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn};
use qinterface::handy::Usc;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use crate::{
    dns::{AGENT, DNS_SERVER, get_or_create_addr_rigistery, resolve_dns},
    parse::{router::Router, rule::Rule, server::ForwardConfig},
    support::TokioIo,
};

pub struct ForwardServer;

static ALPN: &[u8] = b"h3";

impl ForwardServer {
    pub async fn serve(addr: SocketAddr, server: ForwardConfig) {
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
                        .serve_connection(
                            io,
                            service_fn(|req| {
                                // TODO 在此处创建 QUIC 客户端

                                if req.method() == "CONNECT" {
                                    handler_connect(server.addr, router.clone(), req).boxed()
                                } else {
                                    handler(addr, router.clone(), req).boxed()
                                }
                            }),
                        )
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

async fn handler_connect(
    bind: SocketAddr,
    router: Arc<Router>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    info!("CONNECT request");
    if let Some(_addr) = host_addr(req.uri()) {
        tokio::task::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    let upgraded = TokioIo::new(upgraded);

                    tokio::task::spawn(async move {
                        http1::Builder::new()
                            .preserve_header_case(true)
                            .title_case_headers(true)
                            .serve_connection(
                                upgraded.inner(),
                                service_fn(|req| handler(bind, router.clone(), req)),
                            )
                            .await
                            .expect("tunnel server error");
                    });
                }
                Err(e) => eprintln!("upgrade error: {}", e),
            }
        });

        Ok(Response::new(empty()))
    } else {
        warn!("CONNECT host is not socket addr: {:?}", req.uri());
        let mut resp = Response::new(full("CONNECT must be to a socket address"));
        *resp.status_mut() = http::StatusCode::BAD_REQUEST;

        Ok(resp)
    }
}

// TODO 传入 quic connection
async fn handler(
    bind: SocketAddr,
    router: Arc<Router>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let uri = req.uri().to_string();
    info!("request uri: {:?}", uri);

    // 预构建 NOT_FOUND 响应
    let not_found = Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(full(Bytes::new()))
        .expect("static response should be valid");

    debug!("req: {:?}", req);

    // 路由匹配
    let path = req.uri().path().to_owned();
    let (_pattern, rules) = match router.route(path.as_str()) {
        Ok((pattern, rules)) => (pattern, rules),
        Err(_) => return Ok(not_found),
    };

    let rule = if let Rule::Forward(rule) = rules {
        rule
    } else {
        return Ok(not_found);
    };

    let _target = if let Some(target) = &rule.proxy_pass {
        target
    } else {
        return Ok(not_found);
    };

    // 分解请求
    let (parts, body) = req.into_parts();
    let body = body.collect().await?.to_bytes();

    // 获取请求主机头
    let host = match parts.uri.authority().map(|auth| auth.host()) {
        Some(host) => host.to_string(),
        None => match parts.headers.get("host") {
            Some(host) => host.to_str().unwrap().to_string(),
            None => return Ok(not_found),
        },
    };

    debug!("proxy uri host: {}", host);

    // DNS 解析
    let remote = resolve_dns(&host, DNS_SERVER.parse().unwrap())
        .await
        .unwrap();

    info!("dns resolved: {} -> {:?}", host, remote);
    let addr_registry = get_or_create_addr_rigistery(bind).unwrap();
    let outer = addr_registry.outer_addr().await.unwrap();
    let nat_type = addr_registry.nat_type().await.unwrap();

    info!("outer addr {}", outer);
    info!("nat type {:?}", nat_type);
    let usc = Arc::new(Usc::new(addr_registry.iface()).unwrap());
    // 创建并配置 QUIC 客户端
    let bind = addr_registry.bind_addr();
    info!("bind addr: {}", bind);

    let agent: SocketAddr = AGENT;
    let socket = Socket::new(bind, agent);

    let pathway = Pathway::new(Endpoint::Relay { agent, outer }, remote);

    let quic_client = create_quic_client(bind, usc).await;
    // 建立 QUIC 连接
    let conn = quic_client
        .connect(host, socket, pathway)
        .expect("connect quic client");

    let _ = conn.add_address(bind, outer, 1, nat_type);

    // 创建 HTTP/3 客户端
    let (h3_conn, send_request) = create_h3_client(conn.clone()).await;

    // 并发执行连接驱动和请求处理
    let (driver, request) = tokio::join!(
        tokio::spawn(run_quic_driver(h3_conn)),
        tokio::spawn(handle_request(send_request, parts, body))
    );

    // 处理异步任务结果
    driver.expect("driver error").expect("driver result"); // 等待驱动任务完成
    let response = request
        .expect("quic request error")
        .expect("quic request result"); // 获取请求结果

    conn.close(std::borrow::Cow::Borrowed("client done"), 1);

    info!("response uri: {:?}", uri);
    Ok(response)
}

// TODO 多个 forward 实例共享一个 QUIC 客户端

/// 创建 QUIC 客户端
async fn create_quic_client(bind: SocketAddr, usc: Arc<Usc>) -> QuicClient {
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
        .with_keylog(true)
        .with_alpns([ALPN.into()])
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

/// 创建 H3 客户端
async fn create_h3_client(
    conn: Arc<gm_quic::Connection>,
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
    mut sender: h3::client::SendRequest<h3_shim::OpenStreams, Bytes>,
    parts: http::request::Parts,
    body: Bytes,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn std::error::Error + Send + Sync>> {
    let uri = parts.uri.to_string();
    info!("sending request: {}", uri);

    let req = http::Request::from_parts(parts, ());
    let mut stream = sender.send_request(req).await?;
    stream.send_data(body).await?;
    stream.finish().await?;

    let (mut parts, _) = stream.recv_response().await?.into_parts();

    let mut body = Vec::new();
    info!("receiving response body: {}", uri);
    while let Some(chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.chunk());
    }
    info!("received response body: {}", uri);
    parts.version = http::Version::HTTP_11;
    Ok(Response::from_parts(parts, full(Bytes::from(body))))
}

pub fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

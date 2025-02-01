use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Buf, Bytes};
use futures::future;
use gm_quic::{ClientParameters, Pathway, QuicInterface, Socket, prelude::Endpoint};
use h3_shim::QuicClient;
use http::StatusCode;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn};
use qinterface::handy::Usc;
use qtraversal::AddressRegisty;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use crate::{
    parse::{router::Router, rule::Rule, server::ReverseConfig},
    support::TokioIo,
};

#[derive(Clone)]
pub struct ReverseServer;

static ALPN: &[u8] = b"h3";

impl ReverseServer {
    pub async fn serve(addr: SocketAddr, server: ReverseConfig, addr_registry: AddressRegisty) {
        let listener = TcpListener::bind(addr).await.expect("bind tcp listener");
        info!("Listening on http://{}", addr);

        let router = Arc::new(server.router);

        while let Ok((stream, _)) = listener.accept().await {
            let io = TokioIo::new(stream);
            tokio::task::spawn({
                let router = router.clone();
                let addr_registry = addr_registry.clone();
                async move {
                    if let Err(err) = http1::Builder::new()
                        .preserve_header_case(true)
                        .title_case_headers(true)
                        .serve_connection(
                            io,
                            service_fn(|req| handler(router.clone(), req, addr_registry.clone())),
                        )
                        // .with_upgrades()
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
    router: Arc<Router>,
    req: Request<hyper::body::Incoming>,
    addr_registry: AddressRegisty,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
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

    let rule = if let Rule::Reverse(rule) = rules {
        rule
    } else {
        return Ok(not_found);
    };

    if let Some(_target) = &rule.proxy_pass {
        debug!("path: {}", path);

        // 分解请求
        let (parts, body) = req.into_parts();
        let body = body.collect().await?.to_bytes();

        debug!(
            "parts: {:#?} uri.authority: {:#?}",
            parts,
            parts.uri.authority()
        );

        // 获取请求主机头
        // let host = match parts.uri.authority().map(|auth| auth.host()) {
        //     Some(host) => host.to_owned(),
        //     None => return Ok(not_found),
        // };
        let host = path
            .split('/') // 按 '/' 分割字符串
            .nth(1)
            .unwrap();

        debug!("proxy uri host: {}", host);

        // DNS 解析
        let remote = resolve_dns(host, &rule.resolver).await?;
        info!("dns resolved: {} -> {:?}", host, remote);

        let outer = addr_registry.outer_addr().await.unwrap();
        let nat_type = addr_registry.nat_type().await.unwrap();
        // let _addr_changed = addr_registry.keep_alive(Duration::from_secs(30));

        let usc = Arc::new(Usc::new(addr_registry.iface()).unwrap());
        // 创建并配置 QUIC 客户端
        let bind = addr_registry.bind_addr();

        let quic_client = create_quic_client(bind, usc).await;

        let agent: SocketAddr = *remote;
        let pathway = Pathway::new(Endpoint::Relay { agent, outer }, remote);
        let socket = Socket::new(bind, agent);

        // 建立 QUIC 连接
        let conn = quic_client
            .connect(host, socket, pathway)
            .expect("connect quic client");

        let _ = conn.add_address(bind, outer, 1, nat_type);

        // 创建 HTTP/3 客户端
        let (conn, send_request) = create_h3_client(conn).await;

        // 并发执行连接驱动和请求处理
        let (driver, request) = tokio::join!(
            tokio::spawn(run_quic_driver(conn)),
            tokio::spawn(handle_request(send_request, parts, body))
        );

        // 处理异步任务结果
        driver.expect("driver error").expect("driver result"); // 等待驱动任务完成
        let response = request.expect("request error").expect("request result"); // 获取请求结果

        debug!("response: {:#?}", response);
        Ok(response)
    } else {
        Ok(not_found)
    }
}

// DNS 解析示例函数（待实现）
async fn resolve_dns(
    _host: &str,
    resolvers: &Option<Vec<String>>,
) -> Result<Endpoint, hyper::Error> {
    // TODO host decode base64 -> pathway
    // TODO: 实现实际的 DNS 解析逻辑
    // 处理 DNS 解析器
    if let Some(resolvers) = resolvers {
        debug!("using custom resolvers: {:?}", resolvers);
        // TODO 实现自定义 DNS 解析
    } else {
        debug!("using system DNS resolver");
        // TODO 实现系统 DNS 解析
    };
    let endpoint = Endpoint::Relay {
        agent: SocketAddr::from(([1, 12, 74, 4], 20002)),
        outer: SocketAddr::from(([183, 184, 233, 47], 11111)),
    };

    Ok(endpoint)
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

    QuicClient::builder()
        .with_root_certificates(crate::common::root_cert())
        .without_cert()
        .with_keylog(true)
        .with_alpns([ALPN.into()])
        .with_parameters(params)
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
    info!("sending request ...");

    let req = http::Request::from_parts(parts, ());
    let mut stream = sender.send_request(req).await?;
    stream.send_data(body).await?;
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

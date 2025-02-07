use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc};

use bytes::{Buf, Bytes};
use gm_quic::{QuicInterface, prelude::handy::Usc};
use h3::server::RequestStream;
use h3_shim::{BidiStream, QuicServer};
use http::{Request, Response, StatusCode, Uri, Version, response::Parts};
use http_body_util::BodyExt;
use hyper::client::conn::http1::Builder;
use qinterface::path::Endpoint;
use qtraversal::AddressRegisty;
use tokio::net::TcpStream;
use tracing::{debug, error, info};

use crate::{
    AGENT,
    dns::{DNS_SERVER, spwan_report_host_task},
    error::{CustomError, Result},
    forward::full,
    parse::{
        router::Router,
        rule::{ReverseRule, Rule},
        server::ReverseConfig,
    },
    support::TokioIo,
};

static ALPN: &[u8] = b"h3";

#[derive(Clone)]
pub struct ReverseServer;

impl ReverseServer {
    pub async fn serve(
        bind: SocketAddr,
        servers: Vec<ReverseConfig>,
        addr_registry: AddressRegisty,
    ) -> Result<()> {
        debug!("bind: {}, agent: {}", bind, AGENT);
        let outer = addr_registry.outer_addr().await?;
        let nat_type = addr_registry.nat_type().await?;
        let iface = addr_registry.iface();
        debug!("outer: {}, nat_type: {:?}", outer, nat_type);

        let ep = Endpoint::Relay {
            agent: AGENT,
            outer,
        };
        let usc = Arc::new(Usc::new(iface)?);
        let mut routers: HashMap<String, Arc<Router>> = HashMap::new();

        let mut params = gm_quic::ServerParameters::default();

        params.set_initial_max_streams_bidi(100);
        params.set_initial_max_streams_uni(100);
        params.set_initial_max_data((1u32 << 20).into());
        params.set_initial_max_stream_data_uni((1u32 << 20).into());
        params.set_initial_max_stream_data_bidi_local((1u32 << 20).into());
        params.set_initial_max_stream_data_bidi_remote((1u32 << 20).into());

        let mut builder = QuicServer::builder()
            .with_supported_versions([1u32])
            .without_cert_verifier()
            .with_iface_binder(move |addr| {
                if addr == usc.local_addr()? {
                    Ok(usc.clone())
                } else {
                    Ok(Arc::new(Usc::bind(addr)?))
                }
            })
            .with_parameters(params)
            .enable_sni();

        for server in servers.iter() {
            let router = Arc::new(server.router.clone());
            spwan_report_host_task(server.server_name.clone(), ep, DNS_SERVER.parse().unwrap())?;
            for server_name in server.server_name.iter() {
                let cert = std::fs::read(&server.ssl.cert).expect("cannot read cert file");
                let key = std::fs::read(&server.ssl.key).expect("cannot read key file");
                builder = builder.add_host(server_name, &*cert, &*key);
                routers.insert(server_name.clone(), router.clone());
            }
        }

        // TODO 支持范域名路由

        let routers = Arc::new(routers);

        let quic_server = builder.with_alpns([ALPN.to_vec()]).listen(bind)?;

        while let Ok((conn, _pathway)) = quic_server.accept().await {
            debug!(src_addr = ?_pathway.local(), dst_addr = ?_pathway.remote(), "accepted connection");
            let _ = conn.add_address(bind, outer, 1, nat_type);

            let mut conn =
                h3::server::Connection::new(h3_shim::QuicConnection::new(conn).await).await?;
            let routers = routers.clone();
            tokio::spawn({
                async move {
                    while let Ok(Some((req, stream))) = conn.accept().await {
                        tokio::spawn({
                            let routers = routers.clone();
                            async move { handle(routers.clone(), req, stream).await }
                        });
                    }
                }
            });
        }

        Ok(())
    }
}

pub async fn handle(
    routers: Arc<HashMap<String, Arc<Router>>>,
    req: Request<()>,
    stream: RequestStream<BidiStream<Bytes>, Bytes>,
) {
    if let Err(e) = handler_http3(routers, req, stream).await {
        match e {
            CustomError::Unknown => {
                debug!("unknown error");
            }
            _ => {
                debug!("error: {}", e);
            }
        }
    }
}

pub async fn handler_http3(
    routers: Arc<HashMap<String, Arc<Router>>>,
    req: Request<()>,
    mut stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    // 提取主机名
    let host = req
        .uri()
        .authority()
        .ok_or(CustomError::MissingHost)?
        .host();
    let path = req.uri().path();

    let router = routers
        .get(host)
        .ok_or(CustomError::RouterNotFound(host.to_string()))?;
    let (pattern, rules) = router.route(path)?;

    // TODO 解析 rules

    let rule = if let Rule::Reverse(rule) = rules {
        rule
    } else {
        return Err(CustomError::RouterNotFound(path.to_string()));
    };

    let (parts, body) = if let Some(target) = &rule.proxy_pass {
        let (parts, ()) = req.into_parts();

        let mut body = Vec::new();
        while let Some(chunk) = stream.recv_data().await? {
            body.extend_from_slice(chunk.chunk());
        }
        // TODO 添加请求头

        handle_proxy(rule, target, parts, body).await?
    } else if let Some(root) = &rule.root {
        handle_static_file(rule, root, &pattern, path).await?
    } else {
        return Err(CustomError::MissingConfig("proxy_pass or root".to_string()));
    };

    // TODO 添加响应头

    let response = Response::from_parts(parts, ());
    stream.send_response(response).await?;
    if !body.is_empty() {
        stream.send_data(body).await?;
    }
    stream.finish().await?;

    Ok(())
}

pub(super) async fn handle_proxy(
    _rule: &ReverseRule,
    target: &str,
    mut parts: http::request::Parts,
    body: Vec<u8>,
) -> Result<(Parts, Bytes)> {
    info!("proxy to {}", target);

    // 处理代理请求
    parts.uri = Uri::from_str(&format!(
        "{}{}",
        target,
        parts
            .uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default()
    ))?;

    let uri = parts.uri.clone();
    parts.version = Version::HTTP_11;

    let req = Request::from_parts(parts, full(body));
    debug!("req: {:#?}", req);

    // 建立 TCP 连接
    let host = uri.host().ok_or(CustomError::MissingHost)?;
    let port = uri.port().map(|p| p.as_u16()).unwrap_or(80); // 默认端口 80

    let tcp_stream = TcpStream::connect((host, port)).await?;
    let io = TokioIo::new(tcp_stream);

    // 创建 HTTP 连接
    let (mut sender, conn) = Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await?;

    // 异步处理连接
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            error!("Connection failed: {:?}", err);
        }
    });

    // 发送请求并接收响应
    let resp = sender.send_request(req).await?;
    let (parts, body) = resp.into_parts();
    let body = body.collect().await?.to_bytes();
    Ok((parts, body))
}

async fn handle_static_file(
    _rule: &ReverseRule,
    root: &str,
    pattern: &str,
    path: &str,
) -> Result<(Parts, Bytes)> {
    let path = path.replacen(pattern, root, 1);
    info!("Serving static file: {}", path);

    let (status, body) = match std::fs::read(&path) {
        Ok(body) => (StatusCode::OK, Bytes::from(body)),
        Err(_) => (StatusCode::NOT_FOUND, Bytes::new()),
    };

    let (parts, ()) = Response::builder().status(status).body(())?.into_parts();

    Ok((parts, body))
}

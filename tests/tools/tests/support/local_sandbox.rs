//! Explicit localhost test transport for a sandbox rooted in DHTTP_HOME.
use std::{convert::Infallible, error::Error, fmt, sync::Arc};

use bytes::Bytes;
use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use futures::{FutureExt, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use tokio::io::AsyncWriteExt;
type Failure = Box<dyn Error + Send + Sync>;

#[derive(Debug)]
struct LocalPeers;
impl fmt::Display for LocalPeers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("explicit localhost sandbox peers")
    }
}
impl Resolve for LocalPeers {
    fn lookup<'a>(&'a self, name: &'a str, _: &'a str, _: Option<Family>) -> ResolveFuture<'a> {
        async move {
            let port = match name {
                "store.sandbox.dhttp.net" => Some(13444),
                "host.sandbox.dhttp.net" => Some(13445),
                _ => None,
            };
            Ok(futures::stream::iter(port.map(|port| {
                (
                    Source::System,
                    EndpointAddr::direct(([127, 0, 0, 1], port).into()),
                )
            }))
            .boxed())
        }
        .boxed()
    }
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mode = std::env::args().nth(1).unwrap_or_default();
    // This diagnostic never locates the ordinary user home or installs another trust root.
    let root =
        std::env::var_os("DHTTP_HOME").ok_or("DHTTP_HOME is required for the local sandbox")?;
    if !std::path::Path::new(&root).is_absolute() {
        return Err("sandbox home must be absolute".into());
    }
    dhttp::resolve::Resolver::add(Arc::new(LocalPeers));
    dhttp::DhttpNetwork::init().await?;
    let port = if mode == "host" { 13445 } else { 0 };
    let socket = Arc::new(qprotocol::UdpSocket::bind(([127, 0, 0, 1], port).into())?);
    qprotocol::Dock::global()
        .add(socket.clone())?
        .ok_or("sandbox UDP binding already exists")?;
    let _socket = scopeguard::guard(socket, |socket| {
        qprotocol::Dock::global().remove(&socket);
    });
    if mode == "host" {
        return pishoo::run().await.map_err(Into::into);
    }
    if mode != "gateway" {
        return Err("usage: local-sandbox host|gateway".into());
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18080").await?;
    eprintln!("Sandbox HTTP gateway ready on 127.0.0.1:18080");
    loop {
        let (stream, _) = listener.accept().await?;
        {
            let service = hyper::service::service_fn(|request| async move {
                let response = match forward(request).await {
                    Ok(response) => response,
                    Err(error) => {
                        eprintln!("sandbox request failed: {error}");
                        http::Response::builder()
                            .status(502)
                            .body(Full::new(Bytes::from_static(b"local DHTTP request failed")))
                            .unwrap()
                    }
                };
                Ok::<_, Infallible>(response)
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await
            {
                eprintln!("sandbox browser connection: {error}");
            }
        }
    }
}

async fn forward(
    request: http::Request<hyper::body::Incoming>,
) -> Result<http::Response<Full<Bytes>>, Failure> {
    let (mut parts, body) = request.into_parts();
    let identity = parts
        .headers
        .remove("x-sandbox-identity")
        .map(|v| v.to_str().map(str::to_owned))
        .transpose()?
        .unwrap_or_else(|| "host.sandbox.dhttp.net".into());
    let path = parts.uri.path_and_query().map_or("/", |v| v.as_str());
    let (target, path) = if let Some(path) = path.strip_prefix("/registry/") {
        ("store.sandbox.dhttp.net", format!("/{path}"))
    } else {
        ("host.sandbox.dhttp.net", path.to_owned())
    };
    let home = dhttp_home::DhttpHome::load(dhttp_home::HomeScope::User)?;
    let profile = home.identity_profile(&identity)?;
    let endpoint = dhttp::Endpoint::load_from(profile.path()).await?;
    let bytes = Limited::new(body, 64 * 1024 * 1024)
        .collect()
        .await?
        .to_bytes();
    let mut builder = http::Request::builder()
        .method(parts.method)
        .uri(format!("https://{target}{path}"));
    for (name, value) in &parts.headers {
        if !matches!(
            name.as_str(),
            "host"
                | "connection"
                | "proxy-connection"
                | "transfer-encoding"
                | "content-length"
                | "upgrade"
        ) {
            builder = builder.header(name, value);
        }
    }
    let (mut writer, response) = endpoint
        .from_request(builder.body(dhttp::WndBuf::new(64 * 1024))?)
        .await?;
    let send = async {
        writer.write_all(&bytes).await?;
        writer.shutdown().await?;
        Ok::<_, Failure>(())
    };
    let receive = async {
        let response = response.await?;
        let (mut parts, body) = response.into_parts();
        let bytes = Limited::new(body, 64 * 1024 * 1024)
            .collect()
            .await?
            .to_bytes();
        parts.version = http::Version::HTTP_11;
        parts.headers.remove(http::header::TRANSFER_ENCODING);
        parts.headers.remove(http::header::CONNECTION);
        Ok::<_, Failure>(http::Response::from_parts(parts, Full::new(bytes)))
    };
    let (_, response) = tokio::try_join!(send, receive)?;
    Ok(response)
}

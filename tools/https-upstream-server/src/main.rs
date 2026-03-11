use std::{
    convert::Infallible,
    io::Cursor,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use clap::Parser;
use http::header::{CONTENT_TYPE, HeaderValue};
use http_body_util::Full;
use hyper::{Request, Response, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
};
use rustls_pemfile::{certs, private_key};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use x509_parser::{
    extensions::SubjectAlternativeName,
    oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME,
    prelude::{FromDer, GeneralName, X509Certificate, X509Name, parse_x509_certificate},
};

#[derive(Parser, Debug, Clone)]
struct Args {
    /// 本地 HTTPS upstream 监听地址
    #[arg(long, default_value = "127.0.0.1:18443")]
    addr: SocketAddr,
    /// HTTPS upstream 服务端证书
    #[arg(long)]
    server_cert: PathBuf,
    /// HTTPS upstream 服务端私钥
    #[arg(long)]
    server_key: PathBuf,
    /// 用于校验客户端证书的 CA
    #[arg(long)]
    client_ca: PathBuf,
    /// 返回给调用方的固定响应文本，便于脚本断言
    #[arg(long, default_value = "upstream-https-ok")]
    response_text: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // rustls 0.23 需要先安装进程级 crypto provider。
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 1. 读取命令行参数
    // 2. 构造要求客户端证书的 TLS ServerConfig
    // 3. 监听 TCP
    // 4. 对每个连接完成 TLS 握手后交给 hyper HTTP/1.1 服务
    let args = Args::parse();
    let server_config = Arc::new(build_server_config(
        &args.server_cert,
        &args.server_key,
        &args.client_ca,
    )?);
    let acceptor = TlsAcceptor::from(server_config);
    let listener = TcpListener::bind(args.addr).await?;
    let response_text = Arc::new(args.response_text);

    eprintln!("HTTPS upstream listening on https://{}", args.addr);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let response_text = response_text.clone();

        tokio::spawn(async move {
            // 先完成 TLS 握手；若客户端没有带合法证书，会在这里失败。
            let tls_stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("TLS handshake failed from {peer_addr}: {error}");
                    return;
                }
            };

            // 握手成功后，记录客户端证书信息，方便 e2e 脚本断言 pishoo 的 mTLS 行为。
            log_client_certificate(peer_addr, &tls_stream);

            let service = service_fn(move |req| handle_request(req, response_text.clone()));

            // TLS 之上跑一个最小的 HTTP/1.1 服务，仅用于测试链路是否可达。
            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(tls_stream), service)
                .await
            {
                eprintln!("HTTP serving failed for {peer_addr}: {error}");
            }
        });
    }
}

/// 从 TLS 会话中提取客户端证书，并把关键信息打印到日志。
///
/// 日志前缀固定为 `UPSTREAM_CLIENT_CERT`，方便 shell 脚本后处理提取。
fn log_client_certificate(
    peer_addr: SocketAddr,
    tls_stream: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) {
    let (_, session) = tls_stream.get_ref();
    let Some(peer_certs) = session.peer_certificates() else {
        eprintln!("UPSTREAM_CLIENT_CERT peer_addr={peer_addr} missing=true");
        return;
    };

    let Some(leaf_cert) = peer_certs.first() else {
        eprintln!("UPSTREAM_CLIENT_CERT peer_addr={peer_addr} empty=true");
        return;
    };

    match summarize_certificate(leaf_cert) {
        Ok(summary) => eprintln!("UPSTREAM_CLIENT_CERT peer_addr={peer_addr} {summary}"),
        Err(error) => {
            eprintln!("UPSTREAM_CLIENT_CERT peer_addr={peer_addr} parse_error={error}")
        }
    }
}

/// 解析叶子证书，并提取最有用的几个标识字段。
fn summarize_certificate(
    cert: &CertificateDer<'_>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let (_, cert) = parse_x509_certificate(cert.as_ref())?;
    let subject_cn = format_name_common_names(cert.subject());
    let issuer_cn = format_name_common_names(cert.issuer());
    let san_dns = format_subject_alt_names(&cert);
    let serial = cert.raw_serial_as_string();

    Ok(format!(
        "subject_cn={subject_cn} issuer_cn={issuer_cn} san_dns={san_dns} serial={serial}"
    ))
}

/// 提取证书 Subject / Issuer 中的 Common Name，多个值用逗号拼接。
fn format_name_common_names(name: &X509Name<'_>) -> String {
    let common_names = name
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok())
        .collect::<Vec<_>>();

    if common_names.is_empty() {
        "<none>".to_string()
    } else {
        common_names.join(",")
    }
}

/// 提取证书 SAN 扩展中的 DNSName，多个值用逗号拼接。
fn format_subject_alt_names(cert: &X509Certificate<'_>) -> String {
    let Some((_, san)) = cert
        .extensions()
        .iter()
        .find(|ext| ext.oid == OID_X509_EXT_SUBJECT_ALT_NAME)
        .and_then(|ext| SubjectAlternativeName::from_der(ext.value).ok())
    else {
        return "<none>".to_string();
    };

    let dns_names = san
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(value) => Some(value.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();

    if dns_names.is_empty() {
        "<none>".to_string()
    } else {
        dns_names.join(",")
    }
}

/// 返回一个非常简单的 HTTP 文本响应。
///
/// 响应体里会带上 method/path，便于 e2e 脚本确认请求确实到达了 upstream。
async fn handle_request(
    req: Request<Incoming>,
    response_text: Arc<String>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let body = format!("{response_text}\nmethod={}\npath={path}\n", req.method(),);

    let mut response = Response::new(Full::new(Bytes::from(body)));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Ok(response)
}

/// 构建 HTTPS upstream 的服务端 TLS 配置。
///
/// 关键点：
/// - 使用服务端证书和私钥对外提供 HTTPS
/// - 使用 `client_ca` 强制校验客户端证书，从而形成 mTLS
fn build_server_config(
    server_cert: &Path,
    server_key: &Path,
    client_ca: &Path,
) -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let client_roots = Arc::new(load_root_store(client_ca)?);
    let client_verifier = WebPkiClientVerifier::builder(client_roots).build()?;

    let server_config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(load_cert_chain(server_cert)?, load_private_key(server_key)?)?;

    Ok(server_config)
}

/// 把给定 CA 文件中的证书全部加载进 RootCertStore，供客户端证书校验使用。
fn load_root_store(path: &Path) -> Result<RootCertStore, Box<dyn std::error::Error + Send + Sync>> {
    let mut roots = RootCertStore::empty();

    for cert in load_cert_chain(path)? {
        roots.add(cert)?;
    }

    Ok(roots)
}

/// 从 PEM 文件中读取证书链。
///
/// 该函数同时用于：
/// - 读取服务端证书链
/// - 读取客户端 CA 证书
fn load_cert_chain(
    path: &Path,
) -> Result<Vec<CertificateDer<'static>>, Box<dyn std::error::Error + Send + Sync>> {
    let cert_bytes = std::fs::read(path)?;
    let cert_chain = certs(&mut Cursor::new(cert_bytes)).collect::<Result<Vec<_>, _>>()?;

    if cert_chain.is_empty() {
        return Err(format!("no certificates found in {}", path.display()).into());
    }

    Ok(cert_chain)
}

/// 从 PEM 文件中读取私钥。
fn load_private_key(
    path: &Path,
) -> Result<PrivateKeyDer<'static>, Box<dyn std::error::Error + Send + Sync>> {
    let key_bytes = std::fs::read(path)?;
    let mut cursor = Cursor::new(key_bytes);
    let key = private_key(&mut cursor)?
        .ok_or_else(|| format!("no private key found in {}", path.display()))?;

    Ok(key.clone_key())
}

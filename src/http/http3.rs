use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use crate::{
    error::Result,
    handle,
    parse::{router::Router, server::Server},
};
use bytes::Bytes;
use h3::server::RequestStream;
use h3_shim::{BidiStream, QuicConnection, QuicServer};
use http::Request;
use tracing::debug;

use crate::error::CustomError;

static ALPN: &[u8] = b"h3";

#[derive(Clone)]
pub struct H3Server {
    quic_server: Arc<QuicServer>,
    routers: HashMap<String, Router>,
}

impl H3Server {
    pub fn new(bind: SocketAddr, servers: HashMap<String, Server>) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut builder = QuicServer::builder()
            .with_supported_versions([1u32])
            .without_cert_verifier()
            .enable_sni();

        for (server_name, server) in servers.iter() {
            let ssl_config = if let Some(ssl_config) = &server.ssl_config {
                ssl_config
            } else {
                return Err(CustomError::Unknown);
            };

            builder = builder.add_host_with_cert_files(
                server_name,
                &ssl_config.cert_path,
                &ssl_config.key_path,
            )?;
        }

        let quic_server = builder.with_alpns([ALPN.to_vec()]).listen(bind)?;

        let routers = servers
            .into_iter()
            .map(|(name, server)| (name, server.router))
            .collect();

        Ok(Self {
            quic_server,
            routers,
        })
    }

    pub async fn accept(&self) -> Result<h3::server::Connection<QuicConnection, Bytes>> {
        let (conn, _pathway) = self.quic_server.accept().await?;
        debug!(src_addr = %_pathway.local_addr(), dst_addr = %_pathway.dst_addr(), "accepted connection");
        Ok(h3::server::Connection::new(h3_shim::QuicConnection::new(conn).await).await?)
    }

    pub async fn handle(&self, req: Request<()>, stream: RequestStream<BidiStream<Bytes>, Bytes>) {
        if let Err(e) = handle::handler_http3(&self.routers, req, stream).await {
            match e {
                // TODO 这里应该有个统一的错误处理
                CustomError::Unknown => {
                    debug!("unknown error");
                }
                _ => {
                    debug!("error: {}", e);
                }
            }
        }
    }

    pub async fn launch(&mut self) -> Result<()> {
        while let Ok(mut conn) = self.accept().await {
            tokio::spawn({
                let server = self.clone();
                async move {
                    while let Ok(Some((req, stream))) = conn.accept().await {
                        tokio::spawn({
                            let server = server.clone();
                            async move { server.handle(req, stream).await }
                        });
                    }
                }
            });
        }
        Ok(())
    }
}

//! Workspace requests use the Server's named Endpoint and shared connection pool.
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use http::{HeaderMap, Method, StatusCode, Uri, header, uri::PathAndQuery};
use tokio::io::AsyncWriteExt;

const MAX_REMOTE_RESPONSE_BYTES: usize = 1024 * 1024;

pub(crate) struct RemoteResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub remote_subject_id: Vec<u8>,
}

pub(crate) trait OutboundTransport: Send + Sync {
    fn sender_subject_id(&self) -> Result<Option<Vec<u8>>, String>;

    fn request<'a>(
        &'a self,
        target: &'a str,
        method: Method,
        path: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<RemoteResponse, String>>;
}

impl OutboundTransport for dhttp::Endpoint {
    fn sender_subject_id(&self) -> Result<Option<Vec<u8>>, String> {
        let local = self
            .local_authority()
            .map_err(|error| format!("outbound identity unavailable: {error}"))?;
        let identifier =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
                .map_err(|error| format!("outbound identity invalid: {error}"))?;
        Ok(Some(identifier.owner_hash().as_str().as_bytes().to_vec()))
    }

    fn request<'a>(
        &'a self,
        target: &'a str,
        method: Method,
        path: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<RemoteResponse, String>> {
        Box::pin(async move {
            let target = dhttp_home::normalize_name(target)
                .ok_or_else(|| String::from("invalid remote target"))?;
            if !path.starts_with('/') || path.starts_with("//") {
                return Err(String::from("invalid remote request path"));
            }
            let path: PathAndQuery = path
                .parse()
                .map_err(|error| format!("invalid remote request path: {error}"))?;
            let uri = Uri::builder()
                .scheme("https")
                .authority(target.as_str())
                .path_and_query(path)
                .build()
                .map_err(|error| format!("invalid remote URI: {error}"))?;
            tokio::time::timeout(Duration::from_secs(15), async {
                let request = http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(dhttp::WndBuf::with_initial(64 * 1024, body))
                    .map_err(|error| format!("invalid remote request: {error}"))?;
                let (mut writer, response) = self
                    .from_request(request)
                    .await
                    .map_err(|error| format!("remote request failed: {error}"))?;
                let (_, response) = tokio::try_join!(
                    async {
                        writer
                            .shutdown()
                            .await
                            .map_err(|error| format!("remote upload failed: {error}"))
                    },
                    async {
                        response
                            .await
                            .map_err(|error| format!("remote request failed: {error}"))
                    }
                )?;
                let peer = response
                    .extensions()
                    .get::<dhttp::RemoteAuthority>()
                    .ok_or_else(|| String::from("remote identity unavailable"))?;
                if peer.name() != target {
                    return Err(String::from("remote name changed"));
                }
                let identifier = dhttp_home::certificate::extract_dhttp_subject_key_identifier(
                    peer.certificates(),
                )
                .map_err(|error| format!("remote identity invalid: {error}"))?;
                let remote_subject_id = identifier.owner_hash().as_str().as_bytes().to_vec();
                let status = response.status();
                let headers = response.headers().clone();
                let body = axum::body::to_bytes(
                    axum::body::Body::new(response.into_body()),
                    MAX_REMOTE_RESPONSE_BYTES,
                )
                .await
                .map_err(|error| format!("remote response failed: {error}"))?;
                Ok(RemoteResponse {
                    status,
                    headers,
                    body,
                    remote_subject_id,
                })
            })
            .await
            .map_err(|_| String::from("remote request timed out"))?
        })
    }
}

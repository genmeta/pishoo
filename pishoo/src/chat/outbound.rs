//! Chat pins the actual connection peer before any message is sent.
use std::time::Duration;

use access_control::SubjectId;
use bytes::Bytes;
use futures::future::BoxFuture;
use http::{Method, StatusCode, Uri, header, uri::PathAndQuery};
use tokio::io::AsyncWriteExt;

const MAX_REMOTE_RESPONSE_BYTES: usize = 1024 * 1024;

pub(crate) const REMOTE_IDENTITY_CHANGED: &str = "remote Chat peer identity changed";

pub(crate) struct RemoteResponse {
    pub(crate) status: StatusCode,
    pub(crate) body: Bytes,
}

pub(crate) trait OutboundTransport: Send + Sync {
    fn request<'a>(
        &'a self,
        target: &'a str,
        expected_subject_id: &'a SubjectId,
        method: Method,
        path: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<RemoteResponse, String>>;
}

impl OutboundTransport for dhttp::Endpoint {
    fn request<'a>(
        &'a self,
        target: &'a str,
        expected_subject_id: &'a SubjectId,
        method: Method,
        path: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<RemoteResponse, String>> {
        Box::pin(async move {
            let target = dhttp_home::normalize_name(target)
                .ok_or_else(|| String::from("invalid remote target"))?;
            let owner_hash = std::str::from_utf8(expected_subject_id.as_bytes())
                .ok()
                .and_then(|value| dhttp_home::certificate::OwnerHash::try_from(value).ok())
                .ok_or_else(|| String::from(REMOTE_IDENTITY_CHANGED))?;
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
                    .body(dhttp::WndBuf::new(64 * 1024))
                    .map_err(|error| format!("invalid remote request: {error}"))?;
                let (mut writer, response) = self
                    .from_request(request)
                    .expect_remote_owner_hash(owner_hash)
                    .await
                    .map_err(|error| match error {
                        dhttp::Error::RemoteIdentityChanged => {
                            String::from(REMOTE_IDENTITY_CHANGED)
                        }
                        error => format!("remote request failed: {error}"),
                    })?;
                let (_, response) = tokio::try_join!(
                    async {
                        let result = async {
                            writer.write_all(&body).await?;
                            writer.shutdown().await
                        }
                        .await;
                        match result.map_err(dhttp::Error::from) {
                            // The peer may stop receiving and still return a valid response.
                            // Keep reading it; STOP_SENDING alone is not an HTTP result.
                            Err(dhttp::Error::Http3 { source })
                                if matches!(source.as_ref(), h3x::Error::Stream(detail)
                                    if detail.code == h3x::ErrorCode::NoError) =>
                            {
                                Ok(())
                            }
                            result => {
                                result.map_err(|error| format!("remote upload failed: {error}"))
                            }
                        }
                    },
                    async {
                        response
                            .await
                            .map_err(|error| format!("remote request failed: {error}"))
                    }
                )?;
                let status = response.status();
                let body = axum::body::to_bytes(
                    axum::body::Body::new(response.into_body()),
                    MAX_REMOTE_RESPONSE_BYTES,
                )
                .await
                .map_err(|error| format!("remote response failed: {error}"))?;
                Ok(RemoteResponse { status, body })
            })
            .await
            .map_err(|_| String::from("remote request timed out"))?
        })
    }
}

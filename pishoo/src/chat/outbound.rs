use std::{sync::Arc, time::Duration};

use access_control::SubjectId;
use bytes::Bytes;
use dhttp::{
    h3x::{
        connection::ConnectionBuilder,
        dhttp::settings::Settings,
        quic::{self, Connect as _, WithRemoteAuthority as _},
    },
    identity::{Identity, RemoteAuthority as _, RemoteAuthorityCertificateExt as _},
};
use futures::future::BoxFuture;
use gateway::control_plane::{ConnectorRequest, ProvideConnector};
use http::{Method, Request, StatusCode, uri::Authority};
use http_body_util::{BodyExt, Full, Limited};

const MAX_REMOTE_RESPONSE_BYTES: usize = 1024 * 1024;
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);
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

pub(crate) struct PlaneOutbound<P> {
    plane: Arc<P>,
    identity: Identity,
}

impl<P> PlaneOutbound<P> {
    pub(crate) fn new(plane: Arc<P>, identity: Identity) -> Self {
        Self { plane, identity }
    }
}

impl<P> OutboundTransport for PlaneOutbound<P>
where
    P: ProvideConnector + Send + Sync + 'static,
    P::Connector: 'static,
    <P::Connector as quic::Connect>::Connection: 'static,
{
    fn request<'a>(
        &'a self,
        target: &'a str,
        expected_subject_id: &'a SubjectId,
        method: Method,
        path: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<RemoteResponse, String>> {
        Box::pin(async move {
            tokio::time::timeout(REMOTE_TIMEOUT, async {
                let authority: Authority = target
                    .parse()
                    .map_err(|error| format!("invalid target: {error}"))?;
                let connector = self
                    .plane
                    .connector(ConnectorRequest {
                        identity: Some(self.identity.clone()),
                    })
                    .await
                    .map_err(|error| format!("connector unavailable: {error}"))?;
                let quic = connector
                    .connect(&authority)
                    .await
                    .map_err(|error| format!("remote connection failed: {error}"))?;
                let peer = quic
                    .remote_authority()
                    .await
                    .map_err(|error| format!("remote identity unavailable: {error}"))?
                    .ok_or_else(|| String::from("remote identity unavailable"))?;
                let owner_hash = peer
                    .dhttp_subject_key_identifier()
                    .map_err(|error| format!("remote identity invalid: {error}"))?
                    .owner_hash()
                    .to_owned();
                if peer.name() != target
                    || owner_hash.as_str().as_bytes() != expected_subject_id.as_bytes()
                {
                    return Err(String::from(REMOTE_IDENTITY_CHANGED));
                }
                let connection = ConnectionBuilder::new(Arc::new(Settings::default()))
                    .build(quic)
                    .await
                    .map_err(|error| format!("HTTP/3 connection failed: {error}"))?;
                let request = Request::builder()
                    .method(method)
                    .uri(format!("https://{target}{path}"))
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Full::new(body))
                    .map_err(|error| format!("invalid request: {error}"))?;
                let response = connection
                    .execute_hyper_request(request)
                    .await
                    .map_err(|error| format!("remote request failed: {error}"))?;
                let status = response.status();
                let body = Limited::new(response.into_body(), MAX_REMOTE_RESPONSE_BYTES)
                    .collect()
                    .await
                    .map_err(|error| format!("remote response failed: {error}"))?
                    .to_bytes();
                Ok(RemoteResponse { status, body })
            })
            .await
            .map_err(|_| String::from("remote request timed out"))?
        })
    }
}

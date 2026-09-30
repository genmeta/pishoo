use std::{sync::Arc, time::Duration};

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
use http::{HeaderMap, Method, Request, StatusCode, uri::Authority};
use http_body_util::{BodyExt, Full, Limited};

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
    fn sender_subject_id(&self) -> Result<Option<Vec<u8>>, String> {
        self.identity
            .dhttp_subject_key_identifier()
            .map(|identifier| Some(identifier.owner_hash().to_string().into_bytes()))
            .map_err(|error| format!("outbound identity unavailable: {error}"))
    }

    fn request<'a>(
        &'a self,
        target: &'a str,
        method: Method,
        path: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<RemoteResponse, String>> {
        Box::pin(async move {
            tokio::time::timeout(Duration::from_secs(15), async {
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
                let quic = connector.connect(&authority).await.map_err(|error| {
                    format!(
                        "remote connection failed: {}",
                        snafu::Report::from_error(&error)
                    )
                })?;
                let peer = quic
                    .remote_authority()
                    .await
                    .map_err(|error| format!("remote identity unavailable: {error}"))?
                    .ok_or_else(|| String::from("remote identity unavailable"))?;
                if peer.name() != target {
                    return Err(String::from("remote name changed"));
                }
                let remote_subject_id = peer
                    .dhttp_subject_key_identifier()
                    .map_err(|error| format!("remote identity invalid: {error}"))?
                    .owner_hash()
                    .as_str()
                    .as_bytes()
                    .to_vec();
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
                let headers = response.headers().clone();
                let body = Limited::new(response.into_body(), MAX_REMOTE_RESPONSE_BYTES)
                    .collect()
                    .await
                    .map_err(|error| format!("remote response failed: {error}"))?
                    .to_bytes();
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

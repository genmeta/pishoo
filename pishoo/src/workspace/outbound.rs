//! Application transport contract. Production Endpoint wiring is deferred
//! until the underlying identity and connection interfaces are stable.
use bytes::Bytes;
use futures::future::BoxFuture;
use http::{HeaderMap, Method, StatusCode};

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

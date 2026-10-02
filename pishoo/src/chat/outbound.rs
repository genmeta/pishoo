//! Application transport contract. Production Endpoint wiring is deferred
//! until the underlying identity and connection interfaces are stable.
use access_control::SubjectId;
use bytes::Bytes;
use futures::future::BoxFuture;
use http::{Method, StatusCode};

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

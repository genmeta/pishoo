//! Host-controlled outgoing HTTP. Identity comes from authenticated transport
//! metadata, never from guest headers or a destination hostname.
use std::sync::Arc;

use http::Request;
use tokio_util::sync::CancellationToken;
use wasmtime_wasi_http::p2::{
    HttpResult, WasiHttpHooks,
    bindings::http::types::ErrorCode,
    body::HyperOutgoingBody,
    types::{HostFutureIncomingResponse, OutgoingRequestConfig},
};

/// Trusted host transport and target policy, shared by invocations.
/// Implementations must authorize the actual target (including redirects),
/// enforce management-route restrictions, use `source_identity` as the transport
/// identity, and tie all I/O to `cancel`. Self-calls must re-enter normal routing.
pub trait OutgoingHandler: Send + Sync {
    fn send_request(
        &self,
        source_identity: &str,
        request: Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
        cancel: CancellationToken,
    ) -> HttpResult<HostFutureIncomingResponse>;
}

/// Insert into the incoming request's extensions from trusted host middleware.
/// `caller_identity` must be the normalized, authenticated caller identity.
/// This context is consumed before the request is passed to the guest.
#[derive(Clone)]
pub struct OutgoingContext {
    caller_identity: String,
    handler: Arc<dyn OutgoingHandler>,
}

impl OutgoingContext {
    pub fn new(caller_identity: impl Into<String>, handler: Arc<dyn OutgoingHandler>) -> Self {
        Self {
            caller_identity: caller_identity.into(),
            handler,
        }
    }
}

/// Host-authorized outgoing capability. A denied capability still carries the
/// invocation cancellation signal; guest headers can never grant access.
pub struct AuthorizedOutgoing {
    handler: Option<Arc<dyn OutgoingHandler>>,
    cancel: CancellationToken,
}

impl AuthorizedOutgoing {
    /// Only trusted host code may supply this handler; it remains responsible
    /// for checking each actual destination and redirect.
    pub fn new(handler: Arc<dyn OutgoingHandler>, cancel: CancellationToken) -> Self {
        Self {
            handler: Some(handler),
            cancel,
        }
    }

    pub fn denied(cancel: CancellationToken) -> Self {
        Self {
            handler: None,
            cancel,
        }
    }

    pub(crate) fn for_request(
        local: &str,
        context: Option<OutgoingContext>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            handler: context
                .filter(|c| c.caller_identity == local)
                .map(|c| c.handler),
            cancel,
        }
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl OutgoingContext {
    pub fn caller_identity(&self) -> &str {
        &self.caller_identity
    }
}

pub(crate) struct HostOutgoing {
    pub identity: Arc<str>,
    pub authorized: AuthorizedOutgoing,
}

impl WasiHttpHooks for HostOutgoing {
    fn send_request(
        &mut self,
        mut request: Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let handler = self
            .authorized
            .handler
            .as_ref()
            .ok_or(ErrorCode::HttpRequestDenied)?;
        if self.authorized.cancel.is_cancelled() {
            return Err(ErrorCode::HttpRequestDenied.into());
        }
        let reserved: Vec<_> = request
            .headers()
            .keys()
            .filter(|name| name.as_str().starts_with("pishoo-"))
            .cloned()
            .collect();
        for name in reserved {
            request.headers_mut().remove(name);
        }
        handler.send_request(
            &self.identity,
            request,
            config,
            self.authorized.cancel.clone(),
        )
    }
}

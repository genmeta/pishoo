//! Outbound HTTP and identity capabilities exposed to WASM guests.

use http::{Method, Request, Response, Uri};
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};
use wasmtime_wasi_http::p2::{
    HttpResult, WasiHttpHooks,
    bindings::http::types,
    body::HyperOutgoingBody,
    types::{HostFutureIncomingResponse, IncomingResponse, OutgoingRequestConfig},
};

use super::{HostOutgoing, LibPolicy, StoreData};

// Host-authorized outgoing requests use only the current identity Endpoint.

impl WasiHttpHooks for HostOutgoing {
    fn send_request(
        &mut self,
        mut request: Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or(types::ErrorCode::HttpRequestDenied)?
            .clone();
        if self.cancel.is_cancelled()
            || self.remaining_requests == 0
            || !outgoing_allowed(&self.policy, request.method(), request.uri())
        {
            return Err(types::ErrorCode::HttpRequestDenied.into());
        }
        self.remaining_requests -= 1;
        let reserved: Vec<_> = request
            .headers()
            .keys()
            .filter(|name| name.as_str().starts_with("pishoo-"))
            .cloned()
            .collect();
        for name in reserved {
            request.headers_mut().remove(name);
        }
        let cancel = self.cancel.clone();
        let children = self.children.clone();
        let pending = self.children.spawn(async move {
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(Err(types::ErrorCode::HttpRequestDenied)),
                response = tokio::time::timeout(config.connect_timeout.saturating_add(config.first_byte_timeout), endpoint.from_request(request)) => {
                    match response {
                        Ok(Ok(response)) => response,
                        Ok(Err(error)) => return Ok(Err(types::ErrorCode::InternalError(Some(error.to_string())))),
                        Err(_) => return Ok(Err(types::ErrorCode::ConnectionTimeout)),
                    }
                }
            };
            let (parts, mut body) = response.into_parts();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let worker = children.spawn(async move {
                let pumping = async {
                    while let Some(frame) = body.frame().await {
                        match frame {
                            Ok(frame) => match frame.into_data() {
                                Ok(mut bytes) => {
                                    while !bytes.is_empty() {
                                        let chunk = bytes.split_to(bytes.len().min(16 * 1024));
                                        if tx.send(Ok(Frame::data(chunk))).await.is_err() { return; }
                                    }
                                }
                                Err(frame) => { if tx.send(Ok(frame)).await.is_err() { return; } }
                            },
                            Err(error) => {
                                let _ = tx.send(Err(types::ErrorCode::InternalError(Some(error.to_string())))).await;
                                return;
                            }
                        }
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {},
                    _ = pumping => {},
                }
            });
            let frames = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|frame| (frame, rx)) });
            Ok(Ok(IncomingResponse {
                resp: Response::from_parts(parts, StreamBody::new(Box::pin(frames)).boxed_unsync()),
                worker: Some(worker.into()),
                between_bytes_timeout: config.between_bytes_timeout,
            }))
        });
        Ok(HostFutureIncomingResponse::pending(pending.into()))
    }

    fn outgoing_body_buffer_chunks(&mut self) -> usize {
        1
    }
    fn outgoing_body_chunk_size(&mut self) -> usize {
        16 * 1024
    }
}

pub(super) fn outgoing_allowed(policy: &LibPolicy, method: &Method, uri: &Uri) -> bool {
    let Some(host) = uri.host().and_then(dhttp_home::normalize_name) else {
        return false;
    };
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
    {
        return false;
    }
    // Reject ambiguous escaping and normalization before comparing a capability
    // prefix or reserved management route. Guest URLs cannot smuggle dot paths.
    let path = uri.path();
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let Some(high) = bytes.next().and_then(|byte| (byte as char).to_digit(16)) else {
                return false;
            };
            let Some(low) = bytes.next().and_then(|byte| (byte as char).to_digit(16)) else {
                return false;
            };
            let byte = (high * 16 + low) as u8;
            if matches!(byte, b'/' | b'\\' | b'%' | 0) {
                return false;
            }
            decoded.push(byte);
        } else {
            decoded.push(byte);
        }
    }
    let Ok(path) = std::str::from_utf8(&decoded) else {
        return false;
    };
    if path.contains('\\') || path.split('/').any(|part| part == "." || part == "..") {
        return false;
    }
    if [
        "/acl",
        "/contact",
        "/contacts",
        "/workspace",
        "/workspace-api",
    ]
    .iter()
    .any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    }) {
        return false;
    }
    policy.outgoing.iter().any(|rule| {
        rule.methods.contains(method)
            && rule.origin.scheme() == uri.scheme()
            && rule
                .origin
                .host()
                .and_then(dhttp_home::normalize_name)
                .as_ref()
                == Some(&host)
            && rule
                .origin
                .port_u16()
                .or_else(|| match rule.origin.scheme_str() {
                    Some("http") => Some(80),
                    Some("https") => Some(443),
                    _ => None,
                })
                == uri.port_u16().or_else(|| match uri.scheme_str() {
                    Some("http") => Some(80),
                    Some("https") => Some(443),
                    _ => None,
                })
            && rule.path_prefix.starts_with('/')
            && (path == rule.path_prefix
                || path
                    .strip_prefix(&rule.path_prefix)
                    .is_some_and(|rest| rule.path_prefix.ends_with('/') || rest.starts_with('/')))
    })
}

// Existing identity WIT bindings and capability-checked host operations.

pub(super) mod identity {
    wasmtime::component::bindgen!({
        path: "../wit/pishoo-identity",
        inline: "package pishoo:host; world identity-host { import pishoo:identity/signatures@0.1.0; }",
        imports: { default: async },
    });
}

impl identity::pishoo::identity::signatures::Host for StoreData {
    async fn sign(
        &mut self,
        data: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, identity::pishoo::identity::signatures::SignError> {
        use identity::pishoo::identity::signatures::SignError;
        if !self.policy.sign {
            return Err(SignError::Denied);
        }
        if data.len() > 1024 * 1024 {
            return Err(SignError::InputTooLarge);
        }
        if self.outgoing.cancel.is_cancelled() {
            return Err(SignError::Unavailable);
        }
        let signature =
            dhttp::certificate::sign(&self.local, &data).map_err(|_| SignError::Failed)?;
        if signature.len() > 8192 {
            return Err(SignError::Failed);
        }
        Ok(signature)
    }

    async fn verify(
        &mut self,
        signature: Vec<u8>,
        data: Vec<u8>,
        name: String,
    ) -> std::result::Result<bool, identity::pishoo::identity::signatures::VerifyError> {
        use identity::pishoo::identity::signatures::VerifyError;
        if !self.policy.verify {
            return Err(VerifyError::Unavailable);
        }
        if data.len() > 1024 * 1024 || signature.len() > 8192 {
            return Err(VerifyError::InputTooLarge);
        }
        if self.outgoing.cancel.is_cancelled() {
            return Err(VerifyError::Unavailable);
        }
        let name = dhttp_home::normalize_name(&name).ok_or(VerifyError::InvalidIdentity)?;
        if name == self.local.name() {
            return dhttp::certificate::verify_signature(
                self.local.public_key().as_ref(),
                &data,
                &signature,
            )
            .map_err(|_| VerifyError::Failed);
        }
        if let Some(remote) = &self.remote {
            if name == remote.name() {
                return dhttp::certificate::verify_signature(
                    remote.public_key().as_ref(),
                    &data,
                    &signature,
                )
                .map_err(|_| VerifyError::Failed);
            }
        }
        let endpoint = self
            .outgoing
            .endpoint
            .as_ref()
            .ok_or(VerifyError::Unavailable)?;
        let uri = format!("https://{name}/")
            .parse()
            .map_err(|_| VerifyError::InvalidIdentity)?;
        if self.outgoing.remaining_requests == 0
            || !outgoing_allowed(&self.outgoing.policy, &Method::GET, &uri)
        {
            return Err(VerifyError::Unavailable);
        }
        self.outgoing.remaining_requests -= 1;
        let remote = tokio::select! {
            biased;
            _ = self.outgoing.cancel.cancelled() => return Err(VerifyError::Unavailable),
            remote = dhttp::certificate::resolve_remote(endpoint, &name) => remote.map_err(|_| VerifyError::UnknownIdentity)?,
        };
        dhttp::certificate::verify_signature(remote.public_key().as_ref(), &data, &signature)
            .map_err(|_| VerifyError::Failed)
    }
}

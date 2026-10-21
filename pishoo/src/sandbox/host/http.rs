//! Standard WASI HTTP through explicitly granted native resources.
use std::time::Duration;

use ::http::{Request, header};
use http_body_util::{BodyExt, StreamBody};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;
use wasmtime_wasi_http::p2::{
    HttpResult, WasiHttpHooks,
    bindings::http::types,
    body::HyperOutgoingBody,
    types::{HostFutureIncomingResponse, IncomingResponse, OutgoingRequestConfig},
};

use super::super::Outgoing;

impl WasiHttpHooks for Outgoing {
    fn send_request(
        &mut self,
        mut request: Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let Self::Granted {
            endpoint,
            access,
            api,
        } = self
        else {
            return Err(types::ErrorCode::HttpRequestDenied.into());
        };
        let endpoint = endpoint.clone();
        let access = access.clone();
        let api = api.clone();
        if request.uri().scheme_str() != Some("https") {
            return Err(types::ErrorCode::HttpRequestDenied.into());
        }
        let authority = request
            .uri()
            .authority()
            .ok_or(types::ErrorCode::HttpRequestUriInvalid)?;
        if authority.as_str().contains('@') {
            return Err(types::ErrorCode::HttpRequestUriInvalid.into());
        }
        let host = if authority.host() == "~" {
            endpoint.name().to_owned()
        } else {
            authority.host().replace('~', dhttp_home::DHTTP_SUFFIX)
        };
        let target =
            dhttp_home::normalize_name(&host).ok_or(types::ErrorCode::HttpRequestUriInvalid)?;
        let canonical = if let Some(sequence) = authority.port() {
            let sequence = sequence
                .as_str()
                .parse::<u64>()
                .ok()
                .and_then(|n| dhttp_home::certificate::CertificateSequence::try_from(n).ok())
                .ok_or(types::ErrorCode::HttpRequestUriInvalid)?;
            if target == endpoint.name() {
                let local = endpoint.local_authority().map_err(network_error)?;
                let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(
                    local.certificates(),
                )
                .map_err(|_| types::ErrorCode::HttpRequestDenied)?;
                if ski.chain().sequence() != sequence {
                    return Err(types::ErrorCode::HttpRequestDenied.into());
                }
            }
            format!("{target}:{}", sequence.get())
        } else {
            target.clone()
        };
        let mut parts = request.uri().clone().into_parts();
        parts.authority = Some(
            canonical
                .parse()
                .map_err(|_| types::ErrorCode::HttpRequestUriInvalid)?,
        );
        *request.uri_mut() =
            ::http::Uri::from_parts(parts).map_err(|_| types::ErrorCode::HttpRequestUriInvalid)?;
        if request.headers().contains_key(header::TRAILER) {
            return Err(types::ErrorCode::HttpProtocolError.into());
        }
        clean_headers(request.headers_mut());
        request.extensions_mut().clear();
        let waiting = request
            .headers()
            .get("if-reviewed")
            .is_some_and(|value| value == "waiting");
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(if waiting { 300 } else { 15 });
        let (parts, mut body) = request.into_parts();
        let frames = async_stream::try_stream! {
            let mut bytes = 0usize;
            while let Some(frame) = tokio::time::timeout_at(deadline, body.frame()).await
                .map_err(|_| types::ErrorCode::ConnectionTimeout)? {
                let frame = frame?;
                let data = frame.into_data().map_err(|_| types::ErrorCode::HttpProtocolError)?;
                bytes = bytes.saturating_add(data.len());
                if bytes > 1024 * 1024 {
                    Err(types::ErrorCode::HttpRequestBodySize(None))?;
                }
                yield http_body::Frame::data(data);
            }
        };
        let request: Request<HyperOutgoingBody> =
            Request::from_parts(parts, StreamBody::new(Box::pin(frames)).boxed_unsync());
        let future = wasmtime_wasi::runtime::spawn(async move {
            let result = async {
                let (response, worker) = if target == endpoint.name() {
                    let response = tokio::time::timeout_at(
                        deadline.min(tokio::time::Instant::now() + config.first_byte_timeout),
                        api.oneshot(request.map(axum::body::Body::new)),
                    )
                    .await
                    .map_err(|_| types::ErrorCode::ConnectionTimeout)?
                    .unwrap_or_else(|never| match never {});
                    let response = response.map(|body| {
                        body.map_err(|error| {
                            types::ErrorCode::InternalError(Some(error.to_string()))
                        })
                        .boxed_unsync()
                    });
                    (response, None)
                } else {
                    let current = match access.find_contact_by_name(&target).await {
                        Ok(contact) => Some(contact),
                        Err(sea_orm::DbErr::RecordNotFound(_)) => None,
                        Err(error) => {
                            return Err(types::ErrorCode::InternalError(Some(error.to_string())));
                        }
                    };
                    let (mut parts, mut body) = request.into_parts();
                    parts.headers.insert(
                        header::HOST,
                        parts
                            .uri
                            .authority()
                            .unwrap()
                            .as_str()
                            .parse()
                            .map_err(|_| types::ErrorCode::HttpRequestUriInvalid)?,
                    );
                    let mut outgoing = endpoint
                        .from_request(Request::from_parts(parts, dhttp::WndBuf::new(64 * 1024)));
                    if let Some(contact) = current {
                        let owner = std::str::from_utf8(contact.subject_id.as_bytes())
                            .ok()
                            .and_then(|hash| {
                                dhttp_home::certificate::OwnerHash::try_from(hash).ok()
                            })
                            .ok_or(types::ErrorCode::HttpRequestDenied)?;
                        outgoing = outgoing.expect_remote_owner_hash(owner);
                    }
                    let (mut writer, response) = tokio::time::timeout_at(
                        deadline.min(tokio::time::Instant::now() + config.connect_timeout),
                        outgoing,
                    )
                    .await
                    .map_err(|_| types::ErrorCode::ConnectionTimeout)?
                    .map_err(network_error)?;
                    let upload = wasmtime_wasi::runtime::spawn(async move {
                        let result = async {
                            while let Some(frame) = tokio::time::timeout_at(deadline, body.frame())
                                .await
                                .map_err(|_| types::ErrorCode::ConnectionTimeout)?
                            {
                                let frame = frame?;
                                let data = frame
                                    .into_data()
                                    .map_err(|_| types::ErrorCode::HttpProtocolError)?;
                                tokio::time::timeout_at(deadline, writer.write_all(&data))
                                    .await
                                    .map_err(|_| types::ErrorCode::ConnectionTimeout)?
                                    .map_err(|error| {
                                        types::ErrorCode::InternalError(Some(error.to_string()))
                                    })?;
                            }
                            tokio::time::timeout_at(deadline, writer.shutdown())
                                .await
                                .map_err(|_| types::ErrorCode::ConnectionTimeout)?
                                .map_err(|error| {
                                    types::ErrorCode::InternalError(Some(error.to_string()))
                                })
                        }
                        .await;
                        if let Err(error) = result {
                            tracing::warn!(?error, "WASI DHTTP upload stopped");
                        }
                    });
                    let response = tokio::time::timeout_at(
                        deadline.min(tokio::time::Instant::now() + config.first_byte_timeout),
                        response,
                    )
                    .await
                    .map_err(|_| types::ErrorCode::ConnectionTimeout)?
                    .map_err(network_error)?;
                    (
                        response.map(|body| {
                            body.map_err(|error| {
                                types::ErrorCode::InternalError(Some(error.to_string()))
                            })
                            .boxed_unsync()
                        }),
                        Some(upload),
                    )
                };
                let (mut parts, mut body) = response.into_parts();
                clean_headers(&mut parts.headers);
                let frames = async_stream::try_stream! {
                    let mut bytes = 0usize;
                    while let Some(frame) = tokio::time::timeout_at(deadline, body.frame()).await
                        .map_err(|_| types::ErrorCode::HttpResponseTimeout)? {
                        let frame = frame?;
                        if let Some(data) = frame.data_ref() {
                            bytes = bytes.saturating_add(data.len());
                            if bytes > 1024 * 1024 {
                                Err(types::ErrorCode::HttpResponseBodySize(None))?;
                            }
                        }
                        yield frame;
                    }
                };
                Ok(IncomingResponse {
                    resp: ::http::Response::from_parts(
                        parts,
                        StreamBody::new(Box::pin(frames)).boxed_unsync(),
                    ),
                    worker,
                    between_bytes_timeout: config
                        .between_bytes_timeout
                        .min(Duration::from_secs(15)),
                })
            }
            .await;
            Ok(result)
        });
        Ok(HostFutureIncomingResponse::pending(future))
    }
}

fn network_error(error: dhttp::Error) -> types::ErrorCode {
    if matches!(error, dhttp::Error::RemoteIdentityChanged) {
        types::ErrorCode::HttpRequestDenied
    } else {
        types::ErrorCode::InternalError(Some(error.to_string()))
    }
}

fn clean_headers(headers: &mut ::http::HeaderMap) {
    let named = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| name.trim().parse::<::http::HeaderName>().ok())
        .collect::<Vec<_>>();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

use std::{io::Write, net::SocketAddr};

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use chrono::Local;
use http::HeaderValue;

use super::log::AccessLogWriter;

/// Shared state for the access log middleware.
#[derive(Clone)]
pub struct AccessLogState {
    pub writer: AccessLogWriter,
}

/// Axum middleware that logs every request/response in Combined Log Format.
pub async fn access_log(
    State(state): State<AccessLogState>,
    request: Request,
    next: Next,
) -> Response {
    // Capture cheap-to-clone / Copy values before the request is consumed.
    let method = request.method().clone();
    let uri = request.uri().clone();
    let version = request.version();
    let user_agent: Option<HeaderValue> = request.headers().get("user-agent").cloned();
    let referer: Option<HeaderValue> = request.headers().get("referer").cloned();
    let client_addr: Option<SocketAddr> = request.extensions().get::<SocketAddr>().copied();

    let response = next.run(request).await;

    let status = response.status().as_u16();
    let body_size = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    // Format the complete CLF line directly into the writer (one write_all).
    let time_local = Local::now().format("%d/%b/%Y:%H:%M:%S %z");
    let ua = user_agent
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let rf = referer
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");

    // Best-effort: silently drop if the channel is full.
    let mut writer = state.writer.clone();
    let _ = match client_addr {
        Some(addr) => writeln!(
            writer,
            "{} - - [{time_local}] \"{method} {uri} {version:?}\" {status} {body_size} \"{rf}\" \"{ua}\"",
            addr.ip()
        ),
        None => writeln!(
            writer,
            "- - - [{time_local}] \"{method} {uri} {version:?}\" {status} {body_size} \"{rf}\" \"{ua}\""
        ),
    };

    response
}

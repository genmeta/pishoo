use axum::{extract::Request, middleware::Next, response::Response};

use super::log::RequestInfo;

/// Axum middleware that logs every request/response in Combined Log Format.
pub async fn access_log(request: Request, next: Next) -> Response {
    let req_info = RequestInfo::from_request(&request);

    let response = next.run(request).await;

    let status = response.status().as_u16();
    // Content-Length gives us the body size without consuming the body
    let body_size = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    req_info.log_access(status, body_size).await;

    response
}

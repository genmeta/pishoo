mod body;

use std::{net::SocketAddr, sync::Arc};

use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use body::{AccessLogBody, AccessRecordSeed};
use dhttp::log::access::{
    AccessLogRecord, AccessRequestTarget, BodyBytesEmitted, ClientAddress, OptionalReferer,
    OptionalUserAgent,
};

use super::log::AccessLogOutput;

#[derive(Clone, Debug)]
pub enum ActiveAccessLog {
    Disabled,
    Enabled(Arc<AccessLogOutput>),
}

impl ActiveAccessLog {
    pub fn from_output(output: Option<Arc<AccessLogOutput>>) -> Self {
        match output {
            Some(output) => Self::Enabled(output),
            None => Self::Disabled,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AccessLogState {
    pub server: ActiveAccessLog,
}

pub async fn access_log(
    State(state): State<AccessLogState>,
    request: Request,
    next: Next,
) -> Response {
    let seed = RequestSeed::capture(&request);
    let response = next.run(request).await;
    let active = response
        .extensions()
        .get::<ActiveAccessLog>()
        .cloned()
        .unwrap_or(state.server);
    let ActiveAccessLog::Enabled(output) = active else {
        return response;
    };

    let (parts, body) = response.into_parts();
    let record = seed.complete(parts.status);
    if record.has_no_body() {
        output.write(&record.finish(BodyBytesEmitted::ZERO));
        return Response::from_parts(parts, body);
    }

    Response::from_parts(parts, Body::new(AccessLogBody::new(body, output, record)))
}

struct RequestSeed {
    client: ClientAddress,
    method: http::Method,
    target: AccessRequestTarget,
    version: http::Version,
    referer: OptionalReferer,
    user_agent: OptionalUserAgent,
}

impl RequestSeed {
    fn capture(request: &Request) -> Self {
        let client = request
            .extensions()
            .get::<SocketAddr>()
            .map_or(ClientAddress::Unknown, |address| {
                ClientAddress::Ip(address.ip())
            });
        Self {
            client,
            method: request.method().clone(),
            target: AccessRequestTarget::from(request.uri()),
            version: request.version(),
            referer: OptionalReferer::from(request.headers()),
            user_agent: OptionalUserAgent::from(request.headers()),
        }
    }

    fn complete(self, status: http::StatusCode) -> AccessRecordSeed {
        AccessRecordSeed {
            client: self.client,
            method: self.method,
            target: self.target,
            version: self.version,
            referer: self.referer,
            user_agent: self.user_agent,
            status,
        }
    }
}

impl AccessRecordSeed {
    fn has_no_body(&self) -> bool {
        self.method == http::Method::HEAD
            || self.status.is_informational()
            || self.status == http::StatusCode::NO_CONTENT
            || self.status == http::StatusCode::NOT_MODIFIED
    }

    fn finish(self, body_bytes: BodyBytesEmitted) -> AccessLogRecord {
        AccessLogRecord {
            completed_at: chrono::Local::now().fixed_offset(),
            client: self.client,
            method: self.method,
            target: self.target,
            version: self.version,
            status: self.status,
            body_bytes,
            referer: self.referer,
            user_agent: self.user_agent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(method: http::Method, status: http::StatusCode) -> AccessRecordSeed {
        RequestSeed::capture(
            &Request::builder()
                .method(method)
                .uri("/private?token=secret")
                .body(Body::empty())
                .unwrap(),
        )
        .complete(status)
    }

    #[test]
    fn request_capture_discards_query_and_unallowlisted_headers() {
        let mut request = Request::builder()
            .uri("/private?token=secret")
            .header(http::header::AUTHORIZATION, "Bearer secret")
            .header(http::header::COOKIE, "session=secret")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(SocketAddr::from(([127, 0, 0, 1], 443)));

        let record = RequestSeed::capture(&request)
            .complete(http::StatusCode::OK)
            .finish(BodyBytesEmitted::ZERO);

        assert_eq!(record.target.path(), Some("/private"));
        assert_eq!(record.client, ClientAddress::Ip([127, 0, 0, 1].into()));
        assert_eq!(record.referer.value(), None);
        assert_eq!(record.user_agent.value(), None);
    }

    #[test]
    fn head_and_bodyless_statuses_finalize_without_observing_a_body() {
        assert!(record(http::Method::HEAD, http::StatusCode::OK).has_no_body());
        for status in [
            http::StatusCode::CONTINUE,
            http::StatusCode::NO_CONTENT,
            http::StatusCode::NOT_MODIFIED,
        ] {
            assert!(record(http::Method::GET, status).has_no_body());
        }
        assert!(!record(http::Method::GET, http::StatusCode::OK).has_no_body());
    }
}

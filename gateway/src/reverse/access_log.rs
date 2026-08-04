mod body;

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use body::{AccessLogBody, AccessRecordSeed};
use dhttp::{
    log::access::{
        AccessLogRecord, AccessRequestTarget, BodyBytesEmitted, ClientAddress, OptionalReferer,
        OptionalUserAgent,
    },
    name::DhttpName,
};

use super::{access_control::ClientNameResolver, log::AccessLogOutput};

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
    pub client_names: ClientNameResolver,
}

pub async fn access_log(
    State(state): State<AccessLogState>,
    request: Request,
    next: Next,
) -> Response {
    let client_identity = state
        .client_names
        .resolve(&request)
        .await
        .map(|name| short_client_identity(&name));
    let seed = RequestSeed::capture(&request, client_identity);
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
        let (record, client_identity) = record.finish(BodyBytesEmitted::ZERO);
        output.write(&record, client_identity.as_deref());
        return Response::from_parts(parts, body);
    }

    Response::from_parts(parts, Body::new(AccessLogBody::new(body, output, record)))
}

struct RequestSeed {
    client: ClientAddress,
    client_identity: Option<String>,
    method: http::Method,
    target: AccessRequestTarget,
    version: http::Version,
    referer: OptionalReferer,
    user_agent: OptionalUserAgent,
}

impl RequestSeed {
    fn capture(request: &Request, client_identity: Option<String>) -> Self {
        Self {
            client: ClientAddress::Unknown,
            client_identity,
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
            client_identity: self.client_identity,
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

    fn finish(self, body_bytes: BodyBytesEmitted) -> (AccessLogRecord, Option<String>) {
        let record = AccessLogRecord {
            completed_at: chrono::Local::now().fixed_offset(),
            client: self.client,
            method: self.method,
            target: self.target,
            version: self.version,
            status: self.status,
            body_bytes,
            referer: self.referer,
            user_agent: self.user_agent,
        };
        (record, self.client_identity)
    }
}

fn short_client_identity(name: &str) -> String {
    name.strip_suffix(DhttpName::SUFFIX)
        .unwrap_or(name)
        .to_owned()
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
            None,
        )
        .complete(status)
    }

    #[test]
    fn request_capture_discards_query_and_unallowlisted_headers() {
        let request = Request::builder()
            .uri("/private?token=secret")
            .header(http::header::AUTHORIZATION, "Bearer secret")
            .header(http::header::COOKIE, "session=secret")
            .body(Body::empty())
            .unwrap();
        let (record, client_identity) = RequestSeed::capture(&request, None)
            .complete(http::StatusCode::OK)
            .finish(BodyBytesEmitted::ZERO);

        assert_eq!(record.target.path(), Some("/private"));
        assert_eq!(record.client, ClientAddress::Unknown);
        assert_eq!(record.referer.value(), None);
        assert_eq!(record.user_agent.value(), None);
        assert_eq!(client_identity, None);
    }

    #[test]
    fn client_identity_omits_only_the_dhttp_suffix() {
        assert_eq!(
            short_client_identity("reimu.pilot.dhttp.net"),
            "reimu.pilot"
        );
        assert_eq!(short_client_identity("service.example"), "service.example");
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

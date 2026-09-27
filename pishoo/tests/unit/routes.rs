use std::sync::Arc;

use access_control::{AccessService, Action, SubjectId, Visitor};
use axum::{Router, body::Body as AxumBody};
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;

use super::{
    access::{authorize, workspace},
    proxy::{clean_hop_headers, proxy_uri},
    static_file, *,
};
use crate::{
    Error,
    setup::{ProxyLocation, ServerConfig},
};

fn anonymous_request() -> Request<AxumBody> {
    let name = "owner.dhttp.net";
    let cert = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    let local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![cert.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    let mut request = Request::builder()
        .uri("https://owner.dhttp.net/protected?x=1")
        .body(AxumBody::empty())
        .unwrap();
    request.extensions_mut().insert(dhttp::HandshakeSummary {
        alpn: None,
        local: Some(local),
        remote: None,
    });
    // Old middleware or a forwarded request must not supply authentication.
    request.extensions_mut().insert(Visitor::new(
        "forged.dhttp.net",
        SubjectId::new([2]).unwrap(),
    ));
    request
}

async fn authorization_app(effect: access_control::Effect) -> (Arc<AccessService>, Router) {
    let access = Arc::new(
        AccessService::load_from_db(
            "sqlite::memory:",
            "owner.dhttp.net",
            &SubjectId::new([1]).unwrap(),
        )
        .await
        .unwrap(),
    );
    access
        .set_policy(
            access_control::Method::Unspecified,
            "/protected",
            effect,
            access_control::Grantee::Anony,
        )
        .await
        .unwrap();
    let app = Router::new()
        .route(
            "/protected",
            axum::routing::get(|request: Request<AxumBody>| async move {
                assert!(
                    request.extensions().get::<Visitor>().is_none(),
                    "anonymous request retains no stale Visitor"
                );
                StatusCode::NO_CONTENT
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));
    (access, app)
}

async fn pending_review(access: &AccessService) -> u64 {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let (count, reviews) = access.pending_live_reviews(0, 10);
            if count == 1 {
                let access_control::ReviewRecord::Live { id, request } = &reviews[0] else {
                    panic!("expected live review")
                };
                assert!(request.headers().request_id.is_none());
                assert!(request.name().is_none());
                assert_eq!(request.headers().path, "/protected?x=1");
                return *id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("authorization must register its live review")
}

#[path = "routes/authorization.rs"]
mod authorization;
#[path = "routes/routing.rs"]
mod routing;

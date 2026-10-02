use super::*;
use crate::routes::{access_router, authorize, file_router, proxy_pass};

#[tokio::test]
async fn exec_route_is_mounted_inside_the_application_router() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.path()).unwrap();
    let endpoint = dhttp::Endpoint::load(profile.name()).await.unwrap();
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            &access_control::SubjectId::new(b"owner").unwrap(),
        )
        .await
        .unwrap(),
    );
    access
        .set_policy(
            access_control::Method::Unspecified,
            "/exec",
            access_control::Effect::Allow,
            access_control::Grantee::Anony,
        )
        .await
        .unwrap();
    let exec = exec(
        true,
        endpoint.name().to_owned(),
        profile.path().to_path_buf(),
        TaskTracker::new(),
    );
    let app = Router::new()
        .merge(access_router(access.clone()))
        .merge(exec)
        .merge(file_router(profile.join("file")))
        .fallback(any(move |request: Request<AxumBody>| {
            proxy_pass(vec![], request)
        }))
        .layer(axum::middleware::from_fn_with_state(access, authorize));
    let certificate = rcgen::generate_simple_self_signed(vec!["alice.dhttp.net".into()]).unwrap();
    let local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from("alice.dhttp.net"),
        vec![certificate.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(certificate.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    let mut request = Request::builder()
        .method(http::Method::POST)
        .uri("https://alice.dhttp.net/exec")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(AxumBody::from(
            r#"{"program":"/bin/echo","args":["hello"]}"#,
        ))
        .unwrap();
    request.extensions_mut().insert(dhttp::HandshakeSummary {
        alpn: None,
        local: Some(local),
        remote: None,
    });
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::FORBIDDEN);
}

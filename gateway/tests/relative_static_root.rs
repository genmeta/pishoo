use axum::body::Body;
use gateway::{
    parse,
    reverse::router::{NginxRouter, RouterState},
};
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

#[cfg(feature = "sshd")]
struct DummySpawner;

#[cfg(feature = "sshd")]
impl gateway::control_plane::DynSpawnSession for DummySpawner {
    fn spawn_session<'a>(
        &'a self,
        _username: &'a str,
    ) -> futures::future::BoxFuture<
        'a,
        Result<gateway::control_plane::SessionTransport, Box<dyn std::error::Error + Send + Sync>>,
    > {
        Box::pin(async { Err("dummy".into()) })
    }
}

#[cfg(feature = "sshd")]
struct DummyScope;

#[cfg(feature = "sshd")]
impl gateway::reverse::router::DynTaskScope for DummyScope {
    fn token(&self) -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }

    fn spawn(&self, _task: futures::future::BoxFuture<'static, ()>) {}
}

fn dummy_router_state() -> RouterState {
    RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: std::sync::Arc::new(DummySpawner),
        #[cfg(feature = "sshd")]
        task_scope: std::sync::Arc::new(DummyScope),
    }
}

#[tokio::test]
async fn router_serves_static_file_from_config_relative_root() {
    let dir = std::env::temp_dir().join(format!(
        "gateway-relative-static-root-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos(),
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::write(dir.join("fullchain.crt"), "dummy cert").expect("write cert");
    std::fs::write(dir.join("privkey.pem"), "dummy key").expect("write key");
    std::fs::write(
        dir.join("index.html"),
        "<h1>config-relative static root</h1>",
    )
    .expect("write index");
    std::fs::write(
        dir.join("server.conf"),
        "server {\n    listen all 5378;\n    server_name example.com;\n    ssl_certificate ./fullchain.crt;\n    ssl_certificate_key ./privkey.pem;\n    location / { root .; index index.html; }\n}\n",
    )
    .expect("write config");

    let registry = parse::default_registry();
    let document = parse::load_config_file(
        &dir.join("server.conf"),
        &registry,
        parse::registry::BuildOptions::default(),
    )
    .await
    .expect("config should load");

    let server = document.root.children("server").expect("server children")[0].clone();
    let locations = server
        .children("location")
        .expect("location children")
        .to_vec();
    let router = NginxRouter::new(locations, dummy_router_state());

    let response = router
        .oneshot(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("router should respond");

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body should collect")
        .to_bytes();
    assert!(
        std::str::from_utf8(&body)
            .expect("body should be utf-8")
            .contains("config-relative static root")
    );
}

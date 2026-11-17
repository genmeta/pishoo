//! Real component coverage for tuple identity, standard HTTP, and CLI run.
use serde_json::{Value, json};

use super::*;
use crate::test_support::credentials;

fn endpoint() -> dhttp::Endpoint {
    let name = "alice.dhttp.net";
    let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
    let ski = format!("0:0:{}", "a".repeat(64));
    let mut der = vec![4, ski.len() as u8];
    der.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            der,
        ));
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    dhttp::Endpoint::new(
        qbase::endpoint::Endpoint::new(
            name,
            vec![cert.der().clone()],
            qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            vec![1],
        )
        .unwrap(),
    )
}

pub(super) fn host_lib(directory: &Path) -> Arc<Lib> {
    load(
        include_bytes!("../../../../fixtures/pishoo-host-client.wasm"),
        directory,
    )
}

async fn native(endpoint: &dhttp::Endpoint) -> (Arc<access_control::AccessService>, axum::Router) {
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            &access_control::SubjectId::new("a".repeat(64).as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let api = axum::Router::new()
        .route(
            "/std/native",
            axum::routing::get(|| async { "native response" }),
        )
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            crate::routes::authorize,
        ));
    (access, api)
}

#[tokio::test]
async fn wasi_outgoing_sse_uses_idle_deadlines_and_keeps_finite_response_limits() {
    use wasmtime_wasi_http::WasiHttpHooks;
    let endpoint = endpoint();
    let (access, _) = native(&endpoint).await;
    tokio::time::pause();
    for (accept, content_type, large, expected) in [
        ("text/event-stream", "text/event-stream", false, true),
        ("text/event-stream", "text/event-stream", true, true),
        ("text/event-stream;q=0", "text/event-stream", false, false),
        ("text/event-stream", "application/json", false, false),
    ] {
        let api=axum::Router::new().route("/std/native",axum::routing::get(move || async move {
            let frames=async_stream::stream! {
                if large {
                    for _ in 0..3 {
                        yield Ok::<_,std::io::Error>(http_body::Frame::data(Bytes::from(vec![b'x';8*1024*1024])));
                    }
                } else {
                    yield Ok::<_,std::io::Error>(http_body::Frame::data(Bytes::from_static(b":\n\n")));
                }
                tokio::time::sleep(Duration::from_secs(18)).await;
                yield Ok(http_body::Frame::data(Bytes::from_static(b"data: done\n\n")));
            };
            http::Response::builder().header(http::header::CONTENT_TYPE,content_type)
                .body(axum::body::Body::new(StreamBody::new(Box::pin(frames)))).unwrap()
        }));
        let mut outgoing = Outgoing::Granted {
            source: None,
            endpoint: endpoint.clone(),
            access: access.clone(),
            api,
        };
        let request = Request::get("https://~/std/native")
            .header(http::header::ACCEPT, accept)
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            )
            .unwrap();
        let (response, _completion) =
            Box::into_pin(outgoing.send_request(request, None, Box::new(async { Ok(()) })))
                .await
                .unwrap();
        let result = response.into_body().collect().await;
        assert_eq!(
            result.is_ok(),
            expected,
            "{accept}, {content_type}, large={large}"
        );
    }
}

#[tokio::test]
async fn local_service_admission_uses_the_bound_provider_authorization_declaration() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let endpoint = endpoint();
    let (access, _) = native(&endpoint).await;
    let mut target = host_lib(&root.path().join("target"));
    Arc::get_mut(&mut target).unwrap().id = "dev.genmeta.tech.dhttp.net/std/comm/message".into();
    Arc::get_mut(&mut target).unwrap().openapi = serde_json::from_value(json!({
        "openapi":"3.1.0","info":{"title":"Message","version":"2"},
        "paths":{"/comm/message/list":{"get":{}}},"x-pishoo-service-authorization":true
    }))
    .unwrap();
    let mut older = host_lib(&root.path().join("older"));
    Arc::get_mut(&mut older).unwrap().id = target.id.clone();
    Arc::get_mut(&mut older).unwrap().openapi = serde_json::from_value(json!({
        "openapi":"3.1.0","info":{"title":"Message","version":"1"},
        "paths":{"/comm/message/list":{"get":{}}}
    }))
    .unwrap();
    let mut state = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    state.libs.insert(target.id.clone(), target.clone());
    state
        .versions
        .insert((older.id.clone(), "1".into()), older.clone());
    let sandbox = Arc::new(std::sync::RwLock::new(state));
    let router = axum::Router::new()
        .fallback(axum::routing::get(|| async { "provider checks grants" }))
        .layer(axum::middleware::from_fn_with_state(
            access,
            crate::routes::authorize,
        ));
    for (provider, expected) in [(None, 403), (Some(target.clone()), 200), (Some(older), 403)] {
        let mut request =
            Request::get("https://alice.dhttp.net/std/comm/message/list?channel=example")
                .header("pishoo-source-lib", "forged/source")
                .body(axum::body::Body::empty())
                .unwrap();
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(endpoint.local_authority().unwrap()),
            remote: None,
        });
        request.extensions_mut().insert(endpoint.clone());
        request.extensions_mut().insert(sandbox.clone());
        if let Some(provider) = provider {
            let mut source = host_lib(&root.path().join(format!("source-{expected}")));
            Arc::get_mut(&mut source).unwrap().id = "dev.example.dhttp.net/example/source".into();
            Arc::get_mut(&mut source)
                .unwrap()
                .dependencies
                .insert("std/comm/message".into(), provider);
            request.extensions_mut().insert(source);
        }
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
}

#[tokio::test]
async fn identity_v2_is_a_real_component_import_and_anonymous_has_no_caller() {
    let root = tempfile::tempdir().unwrap();
    let lib = host_lib(root.path());
    let endpoint = endpoint();
    let tasks = TaskTracker::new();
    let invocation = Invocation::new(
        lib,
        endpoint.clone(),
        &dhttp::HandshakeSummary {
            alpn: None,
            local: Some(endpoint.local_authority().unwrap().clone()),
            remote: None,
        },
        tasks.clone(),
        Outgoing::Denied,
    )
    .unwrap();
    let response = invocation
        .execute(request("/identity", empty()))
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"local":["alice.dhttp.net","a".repeat(64)],"caller":null})
    );
    reaped(&tasks).await;
}

#[tokio::test]
async fn local_http_keeps_anonymous_authorization_and_cli_run_uses_native_owner() {
    let root = tempfile::tempdir().unwrap();
    let lib = host_lib(root.path());
    let endpoint = endpoint();
    let (access, api) = native(&endpoint).await;
    let tasks = TaskTracker::new();
    let invocation = Invocation::new(
        lib.clone(),
        endpoint.clone(),
        &dhttp::HandshakeSummary {
            alpn: None,
            local: Some(endpoint.local_authority().unwrap().clone()),
            remote: None,
        },
        tasks.clone(),
        Outgoing::Granted {
            source: None,
            endpoint: endpoint.clone(),
            access: access.clone(),
            api: api.clone(),
        },
    )
    .unwrap();
    let response = invocation
        .execute(request("/outgoing", empty()))
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error":"status 403"})
    );
    Invocation::run(
        lib,
        endpoint.clone(),
        tasks.clone(),
        Outgoing::Granted {
            source: None,
            endpoint,
            access,
            api,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(root.path().join("run-result")).unwrap(),
        b"native response"
    );
    reaped(&tasks).await;
}

#[tokio::test]
async fn cli_run_without_outgoing_grant_fails_and_leaves_no_work() {
    let root = tempfile::tempdir().unwrap();
    let tasks = TaskTracker::new();
    assert!(
        Invocation::run(
            host_lib(root.path()),
            endpoint(),
            tasks.clone(),
            Outgoing::Denied
        )
        .await
        .is_err()
    );
    assert!(!root.path().join("run-result").exists());
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "run alone: requires OpenSSL/UDP and owns global TLS/DNS/DHTTP_HOME"]
async fn host_client_real_dhttp_transport_uses_endpoint_credentials() {
    use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
    use futures::{FutureExt, StreamExt};
    #[derive(Debug)]
    struct Peer(EndpointAddr);
    impl std::fmt::Display for Peer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("WASI peer")
        }
    }
    impl Resolve for Peer {
        fn lookup<'a>(&'a self, _: &'a str, _: &'a str, _: Option<Family>) -> ResolveFuture<'a> {
            async move { Ok(futures::stream::iter([(Source::System, self.0)]).boxed()) }.boxed()
        }
    }
    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[
            ("alice", "alice.dhttp.net"),
            ("receiver", "receiver.dhttp.net"),
        ],
    );
    let previous = std::env::var_os("DHTTP_HOME");
    unsafe {
        std::env::set_var("DHTTP_HOME", root.path());
    }
    let _environment = scopeguard::guard(previous, |value| unsafe {
        match value {
            Some(value) => std::env::set_var("DHTTP_HOME", value),
            None => std::env::remove_var("DHTTP_HOME"),
        }
    });
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([qtls::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();
    let peer = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let address = EndpointAddr::direct(peer.local_addr().unwrap());
    qprotocol::Dock::global()
        .add(peer.clone())
        .unwrap()
        .unwrap();
    let _socket = scopeguard::guard(peer, |peer| {
        qprotocol::Dock::global().remove(&peer);
    });
    dhttp::resolve::Resolver::add(Arc::new(Peer(address)));
    let endpoint = dhttp::Endpoint::load("alice").await.unwrap();
    let receiver = dhttp::Endpoint::load("receiver").await.unwrap();
    let local = endpoint.local_authority().unwrap();
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .unwrap();
    let expected_hash = ski.owner_hash().as_str().to_owned();
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            &access_control::SubjectId::new(expected_hash.as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let listener = receiver
        .listen(
            dhttp::Scope::Loopback.into(),
            tower::service_fn(|request: Request<Body>| async move {
                assert_eq!(request.uri().path(), "/std/native");
                let remote = request
                    .extensions()
                    .get::<dhttp::HandshakeSummary>()
                    .unwrap()
                    .remote
                    .as_ref()
                    .unwrap();
                let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(
                    remote.certificates(),
                )
                .unwrap();
                Ok::<_, std::convert::Infallible>(http::Response::new(axum::body::Body::from(
                    json!({"name":remote.name(), "hash":ski.owner_hash().as_str()}).to_string(),
                )))
            }),
        )
        .await
        .unwrap();
    let _listener = scopeguard::guard(tokio::spawn(listener), |task| task.abort());
    let tasks = TaskTracker::new();
    let lib = host_lib(&root.path().join("component-db"));
    let invocation = Invocation::new(
        lib,
        endpoint.clone(),
        &dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: None,
        },
        tasks.clone(),
        Outgoing::Granted {
            source: None,
            endpoint: endpoint.clone(),
            access,
            api: axum::Router::new(),
        },
    )
    .unwrap();
    let mut request = request("/outgoing", empty());
    request
        .headers_mut()
        .insert("x-target", "receiver.dhttp.net".parse().unwrap());
    let response = invocation.execute(request).await.unwrap();
    let document: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let delivered: Value = serde_json::from_str(document["ok"].as_str().unwrap()).unwrap();
    assert_eq!(
        delivered,
        json!({"name":"alice.dhttp.net","hash":expected_hash})
    );
    reaped(&tasks).await;
}

#[tokio::test]
async fn granted_http_streams_before_upload_eof_and_checks_local_certificate_sequence() {
    use wasmtime_wasi_http::{RequestOptions, WasiHttpHooks};
    let endpoint = endpoint();
    let (access, _) = native(&endpoint).await;
    let api = axum::Router::new().route(
        "/echo",
        axum::routing::post(|request: axum::extract::Request| async move {
            http::Response::new(request.into_body())
        }),
    );
    let mut outgoing = Outgoing::Granted {
        source: None,
        endpoint,
        access,
        api,
    };
    let config = || {
        Some(RequestOptions {
            connect_timeout: Some(Duration::from_secs(5)),
            first_byte_timeout: Some(Duration::from_secs(5)),
            between_bytes_timeout: Some(Duration::from_secs(5)),
        })
    };
    let (sender, body) = upload();
    let request = Request::post("https://~/echo")
        .body(
            body.map_err(|e| wasmtime_wasi_http::Error::InternalError(Some(e.to_string())))
                .boxed_unsync(),
        )
        .unwrap();
    let future = outgoing.send_request(request, config(), Box::new(async { Ok(()) }));
    let (mut response, _completion) =
        tokio::time::timeout(Duration::from_secs(5), Box::into_pin(future))
            .await
            .unwrap()
            .unwrap();
    sender
        .send(Ok(Frame::data(Bytes::from_static(b"first chunk"))))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        "first chunk"
    );
    assert!(!sender.is_closed());
    drop(sender);
    assert!(response.body_mut().frame().await.is_none());
    let request = Request::post("https://~:1/echo")
        .body(
            http_body_util::Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap();
    assert!(
        Box::into_pin(outgoing.send_request(request, config(), Box::new(async { Ok(()) })))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cli_run_deadline_cancels_and_reaps_the_store_task() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = endpoint();
    let (access, _) = native(&endpoint).await;
    let api = axum::Router::new().route(
        "/std/native",
        axum::routing::get(|| async { std::future::pending::<&'static str>().await }),
    );
    let tasks = TaskTracker::new();
    tokio::time::pause();
    let result = Invocation::run(
        host_lib(root.path()),
        endpoint.clone(),
        tasks.clone(),
        Outgoing::Granted {
            source: None,
            endpoint,
            access,
            api,
        },
    )
    .await;
    assert!(matches!(result, Err(Error::Deadline)));
    assert!(tasks.is_empty());
    reaped(&tasks).await;
}

#[tokio::test]
async fn outgoing_default_options_preserve_review_wait_and_body_idle_timeout() {
    use wasmtime_wasi_http::{Error as HttpError, WasiHttpHooks};
    let endpoint = endpoint();
    let (access, _) = native(&endpoint).await;
    // SQLite initializes on worker threads; freeze time only after its pool is ready.
    tokio::time::pause();
    let api = axum::Router::new().route(
        "/wait",
        axum::routing::get(|| async {
            tokio::time::sleep(Duration::from_secs(16)).await;
            let frames =
                futures::stream::pending::<std::result::Result<Frame<Bytes>, std::io::Error>>();
            http::Response::new(axum::body::Body::new(StreamBody::new(frames)))
        }),
    );
    let mut outgoing = Outgoing::Granted {
        source: None,
        endpoint,
        access,
        api,
    };
    let request = Request::get("https://~/wait")
        .header("if-reviewed", "waiting")
        .body(
            http_body_util::Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap();
    let (mut response, completion) =
        Box::into_pin(outgoing.send_request(request, None, Box::new(async { Ok(()) })))
            .await
            .unwrap();
    assert!(matches!(
        response.body_mut().frame().await,
        Some(Err(HttpError::HttpResponseTimeout))
    ));
    Box::into_pin(completion).await.unwrap();
}

#[tokio::test]
async fn source_header_is_rebuilt_from_same_identity_direct_lib_extension() {
    let root = tempfile::tempdir().unwrap();
    let lib = host_lib(root.path());
    let endpoint = endpoint();
    let tasks = TaskTracker::new();
    let mut source = host_lib(&root.path().join("source"));
    Arc::get_mut(&mut source).unwrap().id = "dev.genmeta.tech.dhttp.net/std/comm/group".into();
    for trusted in [false, true] {
        let invocation = Invocation::new(
            lib.clone(),
            endpoint.clone(),
            &dhttp::HandshakeSummary {
                alpn: None,
                local: Some(endpoint.local_authority().unwrap().clone()),
                remote: None,
            },
            tasks.clone(),
            Outgoing::Denied,
        )
        .unwrap();
        let mut request = request("/source", empty());
        request.headers_mut().insert(
            "pishoo-source-lib",
            "spoofed/std/comm/group".parse().unwrap(),
        );
        if trusted {
            request.extensions_mut().insert(source.clone());
        }
        let response = invocation.execute(request).await.unwrap();
        let value: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            value["source"],
            if trusted {
                json!(source.id)
            } else {
                Value::Null
            }
        );
    }
    reaped(&tasks).await;
}

#[tokio::test]
async fn wasi_outgoing_discards_upstream_source_headers_and_sets_direct_lib() {
    use wasmtime_wasi_http::WasiHttpHooks;
    let root = tempfile::tempdir().unwrap();
    let source = host_lib(root.path());
    let endpoint = endpoint();
    let (access, _) = native(&endpoint).await;
    let expected = source.id.clone();
    let api = axum::Router::new().route(
        "/echo",
        axum::routing::post(move |request: axum::extract::Request| {
            let expected = expected.clone();
            async move {
                assert!(request.headers().get("pishoo-source-lib").is_none());
                assert_eq!(request.extensions().get::<Arc<Lib>>().unwrap().id, expected);
                "ok"
            }
        }),
    );
    let mut outgoing = Outgoing::Granted {
        source: Some(source),
        endpoint,
        access,
        api,
    };
    let request = Request::post("https://~/echo")
        .header(
            "pishoo-source-lib",
            "dev.genmeta.tech.dhttp.net/std/comm/group",
        )
        .body(
            http_body_util::Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap();
    let (response, _) =
        Box::into_pin(outgoing.send_request(request, None, Box::new(async { Ok(()) })))
            .await
            .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn local_group_directory_authorization_preserves_original_anonymous_visitor() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let mut group = host_lib(root.path());
    Arc::get_mut(&mut group).unwrap().id = "dev.genmeta.tech.dhttp.net/std/comm/group".into();
    let endpoint = endpoint();
    let (access, _) = native(&endpoint).await;
    let api = axum::Router::new()
        .route(
            "/std/contact",
            axum::routing::put(|request: axum::extract::Request| async move {
                assert!(
                    request
                        .extensions()
                        .get::<access_control::Visitor>()
                        .is_none()
                );
                assert!(request.headers().get("pishoo-source-lib").is_none());
                "verified group operation"
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            access,
            crate::routes::authorize,
        ));
    for trusted in [false, true] {
        let mut request = Request::put("https://alice.dhttp.net/std/contact")
            .header(
                "pishoo-source-lib",
                "dev.genmeta.tech.dhttp.net/std/comm/group",
            )
            .extension(dhttp::HandshakeSummary {
                alpn: None,
                local: Some(endpoint.local_authority().unwrap().clone()),
                remote: None,
            })
            .body(axum::body::Body::empty())
            .unwrap();
        if trusted {
            request.extensions_mut().insert(group.clone());
        }
        let response = api.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            if trusted {
                http::StatusCode::OK
            } else {
                http::StatusCode::FORBIDDEN
            }
        );
    }
}

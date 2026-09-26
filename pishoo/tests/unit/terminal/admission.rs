#[test]
fn policy_defaults_and_unknown_fields() {
    let policy: TerminalPolicy = serde_json::from_str("{}").unwrap();
    assert!(!policy.enabled);
    assert!(policy.administrators.is_empty());
    assert!(
        serde_json::from_str::<TerminalPolicy>(r#"{"enabled":false,"run_user":"root"}"#).is_err()
    );
    let manager = TerminalManager::new(policy, Path::new("/does/not/exist")).unwrap();
    assert!(matches!(manager.backend, TerminalBackend::Disabled));
}

#[test]
fn administrator_names_are_normalized_and_unavailable_backend_stays_closed() {
    let manager = TerminalManager::new(
        TerminalPolicy {
            enabled: true,
            administrators: [Arc::from(" Alice "), Arc::from("alice.dhttp.net")].into(),
        },
        Path::new("/does/not/exist"),
    )
    .unwrap();
    assert!(matches!(
        manager.backend,
        TerminalBackend::Unavailable(Error::BackendUnavailable(_))
    ));
    let administrators = manager.administrators.lock().unwrap();
    assert_eq!(administrators.len(), 1);
    assert!(administrators.contains("alice.dhttp.net"));
    assert!(
        TerminalManager::new(
            TerminalPolicy {
                enabled: true,
                administrators: HashSet::new()
            },
            Path::new(".")
        )
        .is_err()
    );
    assert!(
        TerminalManager::new(
            TerminalPolicy {
                enabled: true,
                administrators: [Arc::from("../alice")].into()
            },
            Path::new(".")
        )
        .is_err()
    );
}

#[tokio::test]
async fn disabled_terminal_is_hidden_and_missing_handshake_is_an_error() {
    let manager =
        Arc::new(TerminalManager::new(TerminalPolicy::default(), Path::new(".")).unwrap());
    let response = manager
        .handle(
            "alice",
            CancellationToken::new(),
            Request::new(body(Bytes::new())),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let manager = Arc::new(
        TerminalManager::new(
            TerminalPolicy {
                enabled: true,
                administrators: [Arc::from("alice")].into(),
            },
            Path::new("."),
        )
        .unwrap(),
    );
    assert!(matches!(
        manager
            .handle(
                "alice",
                CancellationToken::new(),
                Request::new(body(Bytes::new()))
            )
            .await,
        Err(Error::MissingHandshake)
    ));
}

#[tokio::test]
async fn revocation_and_shutdown_cancel_only_owned_sessions() {
    let manager = TerminalManager::new(
        TerminalPolicy {
            enabled: true,
            administrators: [Arc::from("alice"), Arc::from("bob")].into(),
        },
        Path::new("."),
    )
    .unwrap();
    let server = CancellationToken::new();
    let alice = server.child_token();
    let bob = server.child_token();
    manager
        .sessions
        .lock()
        .unwrap()
        .insert(1, (Arc::from("alice.dhttp.net"), alice.clone()));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert(2, (Arc::from("bob.dhttp.net"), bob.clone()));
    manager.revoke(" Alice ");
    assert!(alice.is_cancelled());
    assert!(!bob.is_cancelled());
    assert!(!server.is_cancelled());
    assert!(
        !manager
            .administrators
            .lock()
            .unwrap()
            .contains("alice.dhttp.net")
    );
    manager
        .shutdown(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(bob.is_cancelled());
    assert!(!server.is_cancelled());
    assert!(manager.permits.is_closed());
    manager
        .shutdown(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn shutdown_deadline_does_not_abort_owned_cleanup() {
    let manager = TerminalManager::new(TerminalPolicy::default(), Path::new(".")).unwrap();
    let (finish, waiting) = tokio::sync::oneshot::channel();
    let cleaning = manager.tasks.spawn(async move {
        waiting.await.unwrap();
    });
    assert!(matches!(
        manager.shutdown(Instant::now()).await,
        Err(Error::ShutdownDeadline)
    ));
    assert!(!cleaning.is_finished());
    finish.send(()).unwrap();
    cleaning.await.unwrap();
    manager
        .shutdown(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[test]
fn forward_proxy_background_tasks_use_task_scope() {
    let forward = include_str!("../src/forward.rs");
    let normal = include_str!("../src/forward/normal.rs");
    let quic = include_str!("../src/forward/quic.rs");

    for (path, source) in [
        ("gateway/src/forward.rs", forward),
        ("gateway/src/forward/normal.rs", normal),
        ("gateway/src/forward/quic.rs", quic),
    ] {
        assert!(
            !source.contains("tokio::spawn(") && !source.contains("tokio::task::spawn("),
            "{path} must spawn forward proxy background tasks through ForwardTaskSpawner"
        );
    }

    assert!(
        forward.contains("ForwardTaskScope::new()"),
        "forward server future must own a ForwardTaskScope"
    );
    assert!(
        forward.contains("task_scope.shutdown().await"),
        "forward server future must cancel and wait for scoped tasks before returning"
    );
}

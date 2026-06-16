use std::{ffi::CString, path::PathBuf};

use crate::config::{
    PID_FILE_DEFAULT,
    entry::parse_entry_config,
    root::parse_root_config,
    worker_target::{
        Gid, ResolvedWorkerTarget, Uid, WorkerTarget, compute_worker_diff, resolve_worker_targets,
    },
};

fn create_temp_tls_files() -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "pishoo-config-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).expect("create temp tls dir");
    let cert = base.join("server.pem");
    let key = base.join("server.key");
    std::fs::write(&cert, b"dummy cert").expect("write cert fixture");
    std::fs::write(&key, b"dummy key").expect("write key fixture");
    (cert, key)
}

#[test]
fn invalid_workers_type_is_rejected() {
    let failure = gateway::parse::parse_config_str_for_test("pishoo { workers { nested on; } }")
        .expect_err("workers block should be rejected before config parse");
    assert!(!failure.error.to_string().contains('\n'));
}

#[test]
fn extracts_pid_and_workers() {
    let conf = "pishoo { pid /tmp/pishoo-test.pid; workers alice bob; }";
    let parsed = gateway::parse::parse_config_str_for_test(conf).expect("parse config");
    let root = parse_root_config(&parsed.root).expect("parse root config");
    assert_eq!(root.pid_file, PathBuf::from("/tmp/pishoo-test.pid"));
    assert_eq!(root.workers.len(), 2);
    assert_eq!(root.workers[0].username, "alice");
    assert_eq!(root.workers[1].username, "bob");
}

#[test]
fn parse_workers_from_root_config() {
    let conf = "pishoo { workers alice bob; }";
    let parsed = gateway::parse::parse_config_str_for_test(conf).expect("parse config");
    let root = parse_root_config(&parsed.root).expect("parse root config");
    assert_eq!(root.pid_file, PathBuf::from(PID_FILE_DEFAULT));
    assert_eq!(root.workers.len(), 2);
    assert_eq!(root.workers[0].username, "alice");
    assert_eq!(root.workers[1].username, "bob");
}

#[test]
fn resolve_existing_user_target() {
    let me = nix::unistd::User::from_uid(nix::unistd::getuid())
        .expect("resolve current uid")
        .expect("current user exists");
    let workers = vec![WorkerTarget { username: me.name }];
    let resolved = resolve_worker_targets(&workers).expect("resolve user target");
    assert_eq!(resolved.len(), 1);
    assert!(!resolved[0].dir.as_os_str().is_empty());
}

#[test]
fn parse_entry_config_servers_only() {
    let (cert, key) = create_temp_tls_files();
    let conf = format!(
        "pishoo {{ server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root .; }} }} }}",
        cert.display(),
        key.display()
    );
    let parsed = gateway::parse::parse_config_str_for_test(&conf).expect("parse config");
    let entry = parse_entry_config(&parsed.root).expect("parse entry config");
    assert!(entry.workers.is_empty());
    assert_eq!(entry.local_servers.len(), 1);
}

#[test]
fn parse_entry_config_workers_and_servers() {
    let (cert, key) = create_temp_tls_files();
    let conf = format!(
        "pishoo {{ workers alice; server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root .; }} }} }}",
        cert.display(),
        key.display()
    );
    let parsed = gateway::parse::parse_config_str_for_test(&conf).expect("parse config");
    let entry = parse_entry_config(&parsed.root).expect("parse entry config");
    assert_eq!(entry.workers.len(), 1);
    assert_eq!(entry.local_servers.len(), 1);
}

#[test]
fn worker_diff_unchanged_ignores_order() {
    let current = vec![
        ResolvedWorkerTarget {
            uid: Uid::from_raw(1),
            gid: Gid::from_raw(11),
            name: "alice".to_string(),
            passwd: CString::default(),
            gecos: CString::default(),
            dir: PathBuf::from("/tmp/alice"),
            shell: PathBuf::new(),
        },
        ResolvedWorkerTarget {
            uid: Uid::from_raw(2),
            gid: Gid::from_raw(22),
            name: "bob".to_string(),
            passwd: CString::default(),
            gecos: CString::default(),
            dir: PathBuf::from("/tmp/bob"),
            shell: PathBuf::new(),
        },
    ];
    let next = vec![
        ResolvedWorkerTarget {
            uid: Uid::from_raw(2),
            gid: Gid::from_raw(22),
            name: "bob".to_string(),
            passwd: CString::default(),
            gecos: CString::default(),
            dir: PathBuf::from("/tmp/bob"),
            shell: PathBuf::new(),
        },
        ResolvedWorkerTarget {
            uid: Uid::from_raw(1),
            gid: Gid::from_raw(11),
            name: "alice".to_string(),
            passwd: CString::default(),
            gecos: CString::default(),
            dir: PathBuf::from("/tmp/alice"),
            shell: PathBuf::new(),
        },
    ];

    let diff = compute_worker_diff(&current, &next);
    assert_eq!(diff.unchanged.len(), 2);
    assert!(diff.added.is_empty());
    assert!(diff.removed.is_empty());
    assert!(diff.changed.is_empty());
}

#[test]
fn worker_diff_detects_add_remove() {
    let current = vec![ResolvedWorkerTarget {
        uid: Uid::from_raw(1),
        gid: Gid::from_raw(11),
        name: "alice".to_string(),
        passwd: CString::default(),
        gecos: CString::default(),
        dir: PathBuf::from("/tmp/alice"),
        shell: PathBuf::new(),
    }];
    let next = vec![ResolvedWorkerTarget {
        uid: Uid::from_raw(2),
        gid: Gid::from_raw(22),
        name: "bob".to_string(),
        passwd: CString::default(),
        gecos: CString::default(),
        dir: PathBuf::from("/tmp/bob"),
        shell: PathBuf::new(),
    }];

    let diff = compute_worker_diff(&current, &next);
    assert!(diff.unchanged.is_empty());
    assert_eq!(diff.added.len(), 1);
    assert_eq!(diff.added[0].name, "bob");
    assert_eq!(diff.removed.len(), 1);
    assert_eq!(diff.removed[0].name, "alice");
    assert!(diff.changed.is_empty());
}

#[test]
fn worker_diff_detects_uid_change() {
    let current = vec![ResolvedWorkerTarget {
        uid: Uid::from_raw(1000),
        gid: Gid::from_raw(1000),
        name: "alice".to_string(),
        passwd: CString::default(),
        gecos: CString::default(),
        dir: PathBuf::from("/tmp/alice"),
        shell: PathBuf::new(),
    }];
    let next = vec![ResolvedWorkerTarget {
        uid: Uid::from_raw(1001),
        gid: Gid::from_raw(1001),
        name: "alice".to_string(),
        passwd: CString::default(),
        gecos: CString::default(),
        dir: PathBuf::from("/tmp/alice"),
        shell: PathBuf::new(),
    }];

    let diff = compute_worker_diff(&current, &next);
    assert!(diff.unchanged.is_empty());
    assert!(diff.added.is_empty());
    assert!(diff.removed.is_empty());
    assert_eq!(diff.changed.len(), 1);
    assert_eq!(diff.changed[0].0.uid, Uid::from_raw(1000));
    assert_eq!(diff.changed[0].1.uid, Uid::from_raw(1001));
    assert_eq!(diff.changed[0].1.name, "alice");
}

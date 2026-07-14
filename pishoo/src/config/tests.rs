use std::{ffi::CString, path::PathBuf};

use crate::config::{
    PID_FILE_DEFAULT,
    entry::parse_entry_config,
    root::parse_root_config,
    worker_target::{
        AccountDirectory, AccountGroup, Gid, ResolvedWorkerTarget, Uid, WorkerTarget,
        compute_worker_diff, resolve_worker_targets,
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
fn pishoo_keys_query_pid_workers_and_groups_across_crate() {
    let base = std::env::temp_dir().join(format!(
        "pishoo-typed-keys-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos()
    ));
    let source_path = base.join("pishoo.conf");
    let registry = gateway::parse::default_registry();
    let mut parser = gateway::parse::ConfigDocumentParser::new(&registry);
    let gateway::parse::fragment::ParsedConfigDocument::HypervisorRoot(fragment) = parser
        .parse_text(
            "pishoo { pid run/pishoo.pid; workers alice bob; groups admin web; }",
            &source_path,
            gateway::parse::domain::ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect("root config should parse")
    else {
        panic!("expected pishoo fragment");
    };
    let tree = gateway::parse::tree::build_global_tree(&registry, fragment, Vec::new())
        .expect("tree should seal");
    let pishoo = tree.pishoo();

    let pid = pishoo
        .local(gateway::parse::keys::pishoo::PID)
        .expect("pid query")
        .expect("pid should exist");
    let workers = pishoo
        .local(gateway::parse::keys::pishoo::WORKERS)
        .expect("workers query")
        .expect("workers should exist");
    let groups = pishoo
        .local(gateway::parse::keys::pishoo::GROUPS)
        .expect("groups query")
        .expect("groups should exist");

    assert_eq!(pid.as_ref().as_ref(), base.join("run/pishoo.pid"));
    assert_eq!(workers.0, vec!["alice", "bob"]);
    assert_eq!(groups.0, vec!["admin", "web"]);
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
        "pishoo {{ server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root /tmp; }} }} }}",
        cert.display(),
        key.display()
    );
    let parsed = gateway::parse::parse_config_str_for_test(&conf).expect("parse config");
    let entry = crate::config::entry::parse_entry_config_with_directory(
        &parsed.root,
        &FakeAccountDirectory::default(),
    )
    .expect("parse entry config");
    assert!(entry.workers.is_empty());
    assert_eq!(entry.config_services.len(), 1);
}

#[tokio::test]
async fn parse_entry_config_servers_only_from_file_config() {
    let (cert, key) = create_temp_tls_files();
    let dir = cert.parent().expect("temp dir").to_path_buf();
    let conf_path = dir.join("pishoo.conf");
    std::fs::write(
        &conf_path,
        format!(
            "pishoo {{ server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root .; }} }} }}",
            cert.display(),
            key.display(),
        ),
    )
    .expect("write config");

    let registry = gateway::parse::default_registry();
    let parsed = gateway::parse::load_config_file(
        &conf_path,
        &registry,
        gateway::parse::registry::BuildOptions::default(),
    )
    .await
    .expect("parse config");

    let entry = crate::config::entry::parse_entry_config_with_directory(
        &parsed.root,
        &FakeAccountDirectory::default(),
    )
    .expect("parse entry config");
    assert!(entry.workers.is_empty());
    assert_eq!(entry.config_services.len(), 1);

    let location = entry.config_services[0]
        .children("location")
        .expect("location children")[0]
        .clone();
    assert_eq!(
        location
            .require::<gateway::parse::domain::ResolvedConfigPath>("root")
            .expect("root should be typed")
            .as_ref()
            .as_ref(),
        dir,
    );
}

#[test]
fn parse_entry_config_workers_and_servers() {
    let (cert, key) = create_temp_tls_files();
    let conf = format!(
        "pishoo {{ workers alice; server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root /tmp; }} }} }}",
        cert.display(),
        key.display()
    );
    let parsed = gateway::parse::parse_config_str_for_test(&conf).expect("parse config");
    let entry = parse_entry_config(&parsed.root).expect("parse entry config");
    assert_eq!(entry.workers.len(), 1);
    assert_eq!(entry.config_services.len(), 1);
}

#[derive(Default)]
struct FakeAccountDirectory {
    groups: std::collections::HashMap<String, AccountGroup>,
    group_members: std::collections::HashMap<String, Vec<String>>,
}

impl FakeAccountDirectory {
    fn with_group_members(mut self, name: &str, gid: libc::gid_t, members: &[&str]) -> Self {
        self.groups.insert(
            name.to_string(),
            AccountGroup {
                name: name.to_string(),
                gid: Gid::from_raw(gid),
                members: Vec::new(),
            },
        );
        self.group_members.insert(
            name.to_string(),
            members.iter().map(|member| (*member).to_string()).collect(),
        );
        self
    }
}

impl AccountDirectory for FakeAccountDirectory {
    fn group_by_name(
        &self,
        group_name: &str,
    ) -> Result<Option<AccountGroup>, crate::config::ConfigError> {
        Ok(self.groups.get(group_name).cloned())
    }

    fn group_member_usernames(
        &self,
        group_name: &str,
    ) -> Result<Option<Vec<String>>, crate::config::ConfigError> {
        Ok(self.group_members.get(group_name).cloned())
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn default_global_mode_loads_pishoo_group_when_no_worker_directives() {
    let directory =
        FakeAccountDirectory::default().with_group_members("pishoo", 42, &["alice", "bob"]);
    let parsed = gateway::parse::parse_config_str_for_test("pishoo { pid /tmp/pishoo.pid; }")
        .expect("parse config");

    let entry = crate::config::entry::parse_entry_config_with_directory_and_mode(
        &parsed.root,
        &directory,
        crate::config::worker_target::WorkerDiscoveryMode::DefaultGlobalHome,
    )
    .expect("parse entry config");

    let names: Vec<_> = entry
        .workers
        .iter()
        .map(|worker| worker.username.as_str())
        .collect();
    assert_eq!(names, ["alice", "bob"]);
}

#[cfg(target_os = "macos")]
#[test]
fn default_global_mode_loads_www_group_when_no_worker_directives() {
    let directory =
        FakeAccountDirectory::default().with_group_members("_www", 70, &["alice", "bob"]);
    let parsed = gateway::parse::parse_config_str_for_test("pishoo { pid /tmp/pishoo.pid; }")
        .expect("parse config");

    let entry = crate::config::entry::parse_entry_config_with_directory_and_mode(
        &parsed.root,
        &directory,
        crate::config::worker_target::WorkerDiscoveryMode::DefaultGlobalHome,
    )
    .expect("parse entry config");

    let names: Vec<_> = entry
        .workers
        .iter()
        .map(|worker| worker.username.as_str())
        .collect();
    assert_eq!(names, ["alice", "bob"]);
}

#[test]
fn explicit_config_mode_does_not_load_default_pishoo_group() {
    let directory =
        FakeAccountDirectory::default().with_group_members("pishoo", 42, &["alice", "bob"]);
    let parsed = gateway::parse::parse_config_str_for_test("pishoo { pid /tmp/pishoo.pid; }")
        .expect("parse config");

    let entry = crate::config::entry::parse_entry_config_with_directory_and_mode(
        &parsed.root,
        &directory,
        crate::config::worker_target::WorkerDiscoveryMode::ExplicitConfig,
    )
    .expect("parse entry config");

    assert!(entry.workers.is_empty());
}

#[test]
fn default_global_mode_uses_default_group_even_with_config_servers() {
    let (cert, key) = create_temp_tls_files();
    let conf = format!(
        "pishoo {{ server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root /tmp; }} }} }}",
        cert.display(),
        key.display()
    );
    let parsed = gateway::parse::parse_config_str_for_test(&conf).expect("parse config");
    let directory = FakeAccountDirectory::default().with_group_members("pishoo", 42, &["alice"]);

    let entry = crate::config::entry::parse_entry_config_with_directory_and_mode(
        &parsed.root,
        &directory,
        crate::config::worker_target::WorkerDiscoveryMode::DefaultGlobalHome,
    )
    .expect("parse entry config");

    assert_eq!(entry.config_services.len(), 1);
    assert_eq!(entry.workers.len(), 1);
    assert_eq!(entry.workers[0].username, "alice");
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

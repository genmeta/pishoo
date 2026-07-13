use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use dhttp::home::DhttpHome;

use crate::parse::{
    ConfigDocumentParser,
    document::{ConfigDocument, ConfigNode},
    domain::{ConfigDocumentRole, ResolvedConfigPath},
    error::{ConfigDocumentRoleError, ConfigLoadFailure, LoadConfigError},
    fragment::ParsedConfigDocument,
    registry::{CascadePolicy, DuplicatePolicy, ReloadImpact, TransportPolicy, context},
    source::SourceId,
    types::PathConfig,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn create_temp_file(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "gateway_{prefix}_{}_{}.pem",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, "dummy").expect("write temp config fixture");
    path
}

pub(crate) fn cleanup_temp_files(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn parse_doc(conf: &str) -> ConfigDocument {
    crate::parse::parse_config_str_for_test(conf).expect("config should parse")
}

pub(crate) fn first_pishoo(document: &ConfigDocument) -> Arc<ConfigNode> {
    document.root.children("pishoo").expect("pishoo children")[0].clone()
}

pub(crate) fn first_server(document: &ConfigDocument) -> Arc<ConfigNode> {
    first_pishoo(document)
        .children("server")
        .expect("server children")[0]
        .clone()
}

pub(crate) fn build_server_conf(
    server_cert: &Path,
    server_key: &Path,
    server_body: &str,
) -> String {
    format!(
        "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; {} }} }}",
        server_cert.display(),
        server_key.display(),
        server_body
    )
}

pub(crate) fn build_proxy_conf(
    server_cert: &Path,
    server_key: &Path,
    location_body: &str,
) -> String {
    format!(
        r#"
pishoo {{
    server {{
        listen all 5378;
        server_name example.com;
        ssl_certificate {};
        ssl_certificate_key {};
        location /api {{
            {}
        }}
    }}
}}
"#,
        server_cert.display(),
        server_key.display(),
        location_body
    )
}

pub(crate) fn assert_error_chain_display_single_line(error: &(dyn std::error::Error + 'static)) {
    let mut current = Some(error);
    while let Some(error) = current {
        assert!(
            !error.to_string().contains('\n'),
            "error display should be single-line: {}",
            error
        );
        current = error.source();
    }
}

fn parse_role_document(
    text: &str,
    source_path: &Path,
    role: ConfigDocumentRole<'_>,
) -> Result<ParsedConfigDocument, ConfigLoadFailure> {
    let registry = crate::parse::default_registry();
    ConfigDocumentParser::new(&registry).parse_text(text, source_path, role)
}

fn worker_home() -> DhttpHome {
    DhttpHome::new(PathBuf::from("/tmp/worker/.dhttp"))
}

fn worker_source_path() -> &'static Path {
    Path::new("/tmp/worker/.dhttp/pishoo.conf")
}

fn expect_role_error(failure: &ConfigLoadFailure) -> &ConfigDocumentRoleError {
    let LoadConfigError::DocumentRole { source } = &failure.error else {
        panic!("expected document role error, got {:#?}", failure.error);
    };
    source
}

#[test]
fn worker_role_rejects_hypervisor_only_directives() {
    let home = worker_home();
    let failure = parse_role_document(
        "pishoo { pid /run/pishoo.pid; }",
        worker_source_path(),
        ConfigDocumentRole::WorkerPishoo { home: &home },
    )
    .expect_err("worker config must reject supervisor directives");

    let ConfigDocumentRoleError::DirectiveNotAllowed {
        directive, span, ..
    } = expect_role_error(&failure)
    else {
        panic!("expected role-rejected directive");
    };
    assert_eq!(directive.as_str(), "pid");
    assert_eq!(span.document_id(), failure.document_id());
    assert!(!span.is_empty());
}

#[test]
fn worker_role_rejects_direct_server_children() {
    let home = worker_home();
    let failure = parse_role_document(
        "pishoo { server { listen all 443; } }",
        worker_source_path(),
        ConfigDocumentRole::WorkerPishoo { home: &home },
    )
    .expect_err("worker config must reject direct server declarations");

    let ConfigDocumentRoleError::DirectiveNotAllowed {
        directive, span, ..
    } = expect_role_error(&failure)
    else {
        panic!("expected role-rejected directive");
    };
    assert_eq!(directive.as_str(), "server");
    assert_eq!(span.document_id(), failure.document_id());
    assert!(!span.is_empty());
}

#[test]
fn hypervisor_root_requires_exactly_one_pishoo() {
    let source = Path::new("/tmp/root/pishoo.conf");
    for (text, expected) in [("", 0), ("pishoo {} pishoo {}", 2)] {
        let failure = parse_role_document(
            text,
            source,
            ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect_err("hypervisor root must contain exactly one pishoo block");

        let ConfigDocumentRoleError::ExpectedSinglePishoo { found, span, .. } =
            expect_role_error(&failure)
        else {
            panic!("expected pishoo cardinality error");
        };
        assert_eq!(*found, expected);
        assert_eq!(span.document_id(), failure.document_id());
    }
}

#[test]
fn existing_worker_document_requires_exactly_one_pishoo() {
    let home = worker_home();
    for (text, expected) in [("", 0), ("pishoo {} pishoo {}", 2)] {
        let failure = parse_role_document(
            text,
            worker_source_path(),
            ConfigDocumentRole::WorkerPishoo { home: &home },
        )
        .expect_err("an existing worker document must contain exactly one pishoo block");

        let ConfigDocumentRoleError::ExpectedSinglePishoo { found, span, .. } =
            expect_role_error(&failure)
        else {
            panic!("expected pishoo cardinality error");
        };
        assert_eq!(*found, expected);
        assert_eq!(span.document_id(), failure.document_id());
    }
}

#[test]
fn worker_unknown_directive_is_a_configuration_error() {
    let home = worker_home();
    let failure = parse_role_document(
        "pishoo { unknown_setting on; }",
        worker_source_path(),
        ConfigDocumentRole::WorkerPishoo { home: &home },
    )
    .expect_err("worker config must reject unknown directives");

    let report = snafu::Report::from_error(&failure.error).to_string();
    assert!(report.contains("unknown directive `unknown_setting`"));
    assert_eq!(failure.document_id(), failure.source_map.document_id());
    assert!(
        failure
            .diagnostic()
            .to_string()
            .contains("/tmp/worker/.dhttp/pishoo.conf")
    );
}

#[test]
fn default_root_uses_global_home_source_context() {
    let home = DhttpHome::new(PathBuf::from("/tmp/root"));
    let ParsedConfigDocument::HypervisorRoot(fragment) = parse_role_document(
        "pishoo {}",
        Path::new("/tmp/root/pishoo.conf"),
        ConfigDocumentRole::HypervisorRoot { home: Some(&home) },
    )
    .expect("default root should parse") else {
        panic!("expected hypervisor root fragment");
    };

    let source = fragment
        .source_map()
        .get(SourceId(0))
        .expect("root source should exist");
    assert_eq!(
        source.path.as_deref(),
        Some(Path::new("/tmp/root/pishoo.conf"))
    );
    assert_eq!(source.base_dir.as_deref(), Some(Path::new("/tmp/root")));
}

#[test]
fn explicit_root_uses_explicit_file_source_context() {
    let ParsedConfigDocument::HypervisorRoot(fragment) = parse_role_document(
        "pishoo {}",
        Path::new("/tmp/root/pishoo.conf"),
        ConfigDocumentRole::HypervisorRoot { home: None },
    )
    .expect("explicit root should parse") else {
        panic!("expected hypervisor root fragment");
    };

    let source = fragment
        .source_map()
        .get(SourceId(0))
        .expect("root source should exist");
    assert_eq!(
        source.path.as_deref(),
        Some(Path::new("/tmp/root/pishoo.conf"))
    );
    assert_eq!(source.base_dir.as_deref(), Some(Path::new("/tmp/root")));
}

#[test]
fn identity_document_returns_detached_server_fragments() {
    let home = worker_home();
    let name = dhttp::name::DhttpName::try_from("alice.dhttp.net".to_owned())
        .expect("identity name should be valid");
    let profile = home.identity_profile(name);
    let ParsedConfigDocument::IdentityServers(servers) = parse_role_document(
        "server { listen all 443; } server { listen all 444; }",
        Path::new("/tmp/worker/.dhttp/identities/alice/server.conf"),
        ConfigDocumentRole::IdentityServer {
            home: &home,
            profile: &profile,
        },
    )
    .expect("identity server document should parse") else {
        panic!("expected identity server fragments");
    };

    assert_eq!(servers.len(), 2);
    assert!(
        servers
            .iter()
            .all(|server| server.node().parent().is_none())
    );
}

#[test]
fn identity_document_requires_at_least_one_server() {
    let home = worker_home();
    let name = dhttp::name::DhttpName::try_from("alice.dhttp.net".to_owned())
        .expect("identity name should be valid");
    let profile = home.identity_profile(name);
    let failure = parse_role_document(
        "",
        Path::new("/tmp/worker/.dhttp/identities/alice/server.conf"),
        ConfigDocumentRole::IdentityServer {
            home: &home,
            profile: &profile,
        },
    )
    .expect_err("identity server document must not be empty");

    let ConfigDocumentRoleError::MissingIdentityServer { span, .. } = expect_role_error(&failure)
    else {
        panic!("expected missing identity server error");
    };
    assert_eq!(span.document_id(), failure.document_id());
}

#[test]
fn resolved_config_path_is_absolute_and_source_anchored() {
    let include_dir = PathBuf::from("/tmp/root/includes");
    let include_name = format!(
        "paths-{}-{}.conf",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    );
    let include_path = include_dir.join(&include_name);
    let destination = include_dir.join("run/pishoo.pid");
    std::fs::create_dir_all(&include_dir).expect("create include fixture directory");
    let _ = std::fs::remove_file(&destination);
    std::fs::write(&include_path, "pid run/pishoo.pid;").expect("write include fixture");

    let text = format!("pishoo {{ include includes/{include_name}; }}");
    let ParsedConfigDocument::HypervisorRoot(fragment) = parse_role_document(
        &text,
        Path::new("/tmp/root/pishoo.conf"),
        ConfigDocumentRole::HypervisorRoot { home: None },
    )
    .expect("included path should parse") else {
        panic!("expected hypervisor root fragment");
    };
    let path = fragment
        .node()
        .require::<PathConfig>("pid")
        .expect("pid should be typed")
        .0
        .clone();
    let resolved = ResolvedConfigPath::try_from(path).expect("path should be resolved");

    assert_eq!(resolved.as_ref(), destination);
    assert!(resolved.as_ref().is_absolute());
    assert!(
        !destination.exists(),
        "parser must not create the destination"
    );
    assert!(ResolvedConfigPath::try_from(PathBuf::from("relative/path")).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        let nul_path = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/nul\0path".to_vec()));
        assert!(ResolvedConfigPath::try_from(nul_path).is_err());
    }

    std::fs::remove_file(include_path).expect("remove include fixture");
}

#[test]
fn document_ids_keep_equal_source_spans_distinct() {
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let ParsedConfigDocument::HypervisorRoot(root) = parser
        .parse_text(
            "pishoo {}",
            Path::new("/tmp/root/pishoo.conf"),
            ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect("root document should parse")
    else {
        panic!("expected hypervisor root fragment");
    };
    let home = worker_home();
    let ParsedConfigDocument::WorkerPishoo(worker) = parser
        .parse_text(
            "pishoo {}",
            worker_source_path(),
            ConfigDocumentRole::WorkerPishoo { home: &home },
        )
        .expect("worker document should parse")
    else {
        panic!("expected worker pishoo fragment");
    };

    assert_eq!(root.span().start(), worker.span().start());
    assert_eq!(root.span().end(), worker.span().end());
    assert_ne!(root.span(), worker.span());
    assert_ne!(root.document_id(), worker.document_id());
}

#[test]
fn registry_v1_metadata_is_orthogonal() {
    let registry = crate::parse::default_registry();
    for name in ["pid", "workers", "groups"] {
        let spec = registry
            .directive_spec(context::PISHOO, name)
            .expect("supervisor directive should be registered");
        assert_eq!(spec.duplicate, DuplicatePolicy::Reject);
        assert_eq!(spec.cascade, CascadePolicy::None);
        assert_eq!(spec.transport, TransportPolicy::HypervisorOnly);
        assert_eq!(spec.reload, ReloadImpact::Supervisor);
    }
    for name in [
        "access_rules",
        "gzip",
        "gzip_vary",
        "gzip_min_length",
        "gzip_comp_level",
        "gzip_types",
        "default_type",
    ] {
        let spec = registry
            .directive_spec(context::PISHOO, name)
            .expect("runtime default should be registered");
        assert_eq!(spec.duplicate, DuplicatePolicy::Reject);
        assert_eq!(spec.cascade, CascadePolicy::NearestWins);
        assert_eq!(spec.transport, TransportPolicy::WorkerInheritable);
        assert_eq!(spec.reload, ReloadImpact::RuntimeState);
    }
    let types = registry
        .directive_spec(context::PISHOO, "types")
        .expect("types should be registered");
    assert_eq!(types.cascade, CascadePolicy::ReplaceWhole);

    let server = registry
        .directive_spec(context::PISHOO, "server")
        .expect("root server declaration should be registered");
    assert_eq!(server.duplicate, DuplicatePolicy::Append);
    assert_eq!(server.cascade, CascadePolicy::None);
    assert_eq!(server.transport, TransportPolicy::HypervisorOnly);
    assert_eq!(server.reload, ReloadImpact::ListenerSet);
}

#[test]
fn document_id_index_conversion_rejects_overflow() {
    if usize::BITS > u32::BITS {
        assert!(
            crate::parse::domain::ConfigDocumentId::try_from_index(u32::MAX as usize + 1).is_err()
        );
    }
}

#[test]
fn loose_document_exposes_document_namespace() {
    let document = parse_doc("pishoo {}");

    assert_eq!(document.document_id(), document.source_map.document_id());
}

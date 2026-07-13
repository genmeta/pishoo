use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use dhttp::home::DhttpHome;

use crate::parse::{
    ConfigDocumentParser,
    cascade::{GZIP, GZIP_TYPES, TYPES},
    document::{ConfigDocument, ConfigNode},
    domain::{ConfigDocumentRole, ConfigDocumentRoleKind, ResolvedConfigPath},
    error::{ConfigDocumentRoleError, ConfigLoadFailure, LoadConfigError},
    fragment::{ParsedConfigDocument, ParsedPishooFragment, ParsedServerFragment},
    registry::{
        BuildOptions, CascadePolicy, ContextSpec, DirectiveSpec, DuplicatePolicy, ReloadImpact,
        TransportPolicy, context,
    },
    snapshot::{RootConfigSnapshot, test_support as snapshot_test_support},
    source::SourceId,
    tree::{AttachedConfigNode, ParentLink, build_global_tree, build_worker_tree},
    types::{BoolConfig, MimeTypes, PathConfig, StringList},
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
static LOCAL_FINALIZER_CALLS: AtomicUsize = AtomicUsize::new(0);
static ATTACHED_FINALIZER_CALLS: AtomicUsize = AtomicUsize::new(0);

fn count_local_server_finalizer(
    _node: &mut ConfigNode,
    _options: &BuildOptions<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    LOCAL_FINALIZER_CALLS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn assert_attached_server_context(
    node: AttachedConfigNode<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let parent = node
        .parent()
        .ok_or_else(|| std::io::Error::other("server must be attached before finalization"))?;
    if parent.context() != context::PISHOO {
        return Err(std::io::Error::other("server parent must be PISHOO").into());
    }
    if parent
        .children()
        .filter(|child| child.context() == context::SERVER)
        .count()
        != 2
    {
        return Err(std::io::Error::other("finalizer must observe every attached sibling").into());
    }
    ATTACHED_FINALIZER_CALLS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[derive(Debug)]
struct TempConfigDir {
    path: PathBuf,
}

impl TempConfigDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gateway_parse_{prefix}_{}_{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temporary config fixture directory");
        Self { path }
    }

    fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.path.join(path)
    }

    fn worker_home(&self) -> DhttpHome {
        DhttpHome::new(self.join(".dhttp"))
    }

    fn worker_source_path(&self) -> PathBuf {
        self.join(".dhttp/pishoo.conf")
    }
}

impl Drop for TempConfigDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

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

fn parse_root_fragment(
    parser: &mut ConfigDocumentParser<'_>,
    text: &str,
    source_path: &Path,
    home: Option<&DhttpHome>,
) -> ParsedPishooFragment {
    let ParsedConfigDocument::HypervisorRoot(fragment) = parser
        .parse_text(
            text,
            source_path,
            ConfigDocumentRole::HypervisorRoot { home },
        )
        .expect("root document should parse")
    else {
        panic!("expected root pishoo fragment");
    };
    fragment
}

fn parse_worker_fragment(
    parser: &mut ConfigDocumentParser<'_>,
    text: &str,
    source_path: &Path,
    home: &DhttpHome,
) -> ParsedPishooFragment {
    let ParsedConfigDocument::WorkerPishoo(fragment) = parser
        .parse_text(text, source_path, ConfigDocumentRole::WorkerPishoo { home })
        .expect("worker document should parse")
    else {
        panic!("expected worker pishoo fragment");
    };
    fragment
}

fn parse_identity_fragments(
    parser: &mut ConfigDocumentParser<'_>,
    text: &str,
    source_path: &Path,
    home: &DhttpHome,
    identity: &str,
) -> Vec<ParsedServerFragment> {
    let name = dhttp::name::DhttpName::try_from(identity.to_owned())
        .expect("identity fixture name should be valid");
    let profile = home.identity_profile(name);
    let ParsedConfigDocument::IdentityServers(servers) = parser
        .parse_text(
            text,
            source_path,
            ConfigDocumentRole::IdentityServer {
                home,
                profile: &profile,
            },
        )
        .expect("identity document should parse")
    else {
        panic!("expected identity server fragments");
    };
    servers.into_vec()
}

fn root_snapshot_fixture(
    registry: &crate::parse::registry::ConfigRegistry,
    parser: &mut ConfigDocumentParser<'_>,
    fixture: &TempConfigDir,
) -> RootConfigSnapshot {
    let root = parse_root_fragment(
        parser,
        "pishoo { gzip on; gzip_vary on; gzip_min_length 42; gzip_comp_level 4; types { text/root root; } }",
        &fixture.join("root/pishoo.conf"),
        None,
    );
    let snapshot = build_global_tree(registry, root, Vec::new())
        .expect("root tree should seal")
        .root_snapshot()
        .expect("root snapshot should project");
    snapshot_test_support::checked_wire_round_trip(&snapshot)
        .expect("root snapshot wire round trip should remain checked")
}

fn expect_role_error(failure: &ConfigLoadFailure) -> &ConfigDocumentRoleError {
    let LoadConfigError::DocumentRole { source } = &failure.error else {
        panic!("expected document role error, got {:#?}", failure.error);
    };
    source
}

#[test]
fn worker_role_rejects_hypervisor_only_directives() {
    let fixture = TempConfigDir::new("worker_hypervisor_only");
    let home = fixture.worker_home();
    let source_path = fixture.worker_source_path();
    let failure = parse_role_document(
        "pishoo { pid /run/pishoo.pid; }",
        &source_path,
        ConfigDocumentRole::WorkerPishoo { home: &home },
    )
    .expect_err("worker config must reject supervisor directives");

    let ConfigDocumentRoleError::DirectiveNotAllowed {
        directive,
        role,
        span,
    } = expect_role_error(&failure)
    else {
        panic!("expected role-rejected directive");
    };
    assert_eq!(directive.as_str(), "pid");
    assert_eq!(Some(span.document_id()), failure.document_id());
    assert_eq!(*role, ConfigDocumentRoleKind::WorkerPishoo);
    assert!(!span.is_empty());
    let cause = std::error::Error::source(&failure.error).expect("role error should be the cause");
    let role_cause = cause
        .downcast_ref::<ConfigDocumentRoleError>()
        .expect("load error should expose the genuine role error cause");
    assert!(std::error::Error::source(role_cause).is_none());
    assert!(
        failure
            .diagnostic()
            .to_string()
            .contains(&source_path.display().to_string())
    );
}

#[test]
fn worker_role_rejects_direct_server_children() {
    let fixture = TempConfigDir::new("worker_server_child");
    let home = fixture.worker_home();
    let source_path = fixture.worker_source_path();
    let failure = parse_role_document(
        "pishoo { server { listen all 443; } }",
        &source_path,
        ConfigDocumentRole::WorkerPishoo { home: &home },
    )
    .expect_err("worker config must reject direct server declarations");

    let ConfigDocumentRoleError::DirectiveNotAllowed {
        directive,
        role,
        span,
    } = expect_role_error(&failure)
    else {
        panic!("expected role-rejected directive");
    };
    assert_eq!(directive.as_str(), "server");
    assert_eq!(Some(span.document_id()), failure.document_id());
    assert_eq!(*role, ConfigDocumentRoleKind::WorkerPishoo);
    assert!(!span.is_empty());
}

#[test]
fn hypervisor_root_requires_exactly_one_pishoo() {
    let fixture = TempConfigDir::new("root_pishoo_cardinality");
    let source = fixture.join("pishoo.conf");
    for (text, expected) in [("", 0), ("pishoo {} pishoo {}", 2)] {
        let failure = parse_role_document(
            text,
            &source,
            ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect_err("hypervisor root must contain exactly one pishoo block");

        let ConfigDocumentRoleError::ExpectedSinglePishoo {
            role, found, span, ..
        } = expect_role_error(&failure)
        else {
            panic!("expected pishoo cardinality error");
        };
        assert_eq!(*found, expected);
        assert_eq!(Some(span.document_id()), failure.document_id());
        assert_eq!(*role, ConfigDocumentRoleKind::HypervisorRoot);
    }
}

#[test]
fn existing_worker_document_requires_exactly_one_pishoo() {
    let fixture = TempConfigDir::new("worker_pishoo_cardinality");
    let home = fixture.worker_home();
    let source_path = fixture.worker_source_path();
    for (text, expected) in [("", 0), ("pishoo {} pishoo {}", 2)] {
        let failure = parse_role_document(
            text,
            &source_path,
            ConfigDocumentRole::WorkerPishoo { home: &home },
        )
        .expect_err("an existing worker document must contain exactly one pishoo block");

        let ConfigDocumentRoleError::ExpectedSinglePishoo {
            role, found, span, ..
        } = expect_role_error(&failure)
        else {
            panic!("expected pishoo cardinality error");
        };
        assert_eq!(*found, expected);
        assert_eq!(Some(span.document_id()), failure.document_id());
        assert_eq!(*role, ConfigDocumentRoleKind::WorkerPishoo);
    }
}

#[test]
fn worker_unknown_directive_is_a_configuration_error() {
    let fixture = TempConfigDir::new("worker_unknown");
    let home = fixture.worker_home();
    let source_path = fixture.worker_source_path();
    let failure = parse_role_document(
        "pishoo { unknown_setting on; }",
        &source_path,
        ConfigDocumentRole::WorkerPishoo { home: &home },
    )
    .expect_err("worker config must reject unknown directives");

    let report = snafu::Report::from_error(&failure.error).to_string();
    assert!(report.contains("unknown directive `unknown_setting`"));
    assert!(failure.document_id().is_some());
    assert!(
        failure
            .diagnostic()
            .to_string()
            .contains(&source_path.display().to_string())
    );
}

#[test]
fn default_root_uses_global_home_source_context() {
    let fixture = TempConfigDir::new("default_root_source");
    let home = DhttpHome::new(fixture.path.clone());
    let source_path = fixture.join("pishoo.conf");
    let ParsedConfigDocument::HypervisorRoot(fragment) = parse_role_document(
        "pishoo { server { listen all 443; } }",
        &source_path,
        ConfigDocumentRole::HypervisorRoot { home: Some(&home) },
    )
    .expect("default root should parse") else {
        panic!("expected hypervisor root fragment");
    };

    let source = fragment
        .source_map()
        .get(SourceId(0))
        .expect("root source should exist");
    assert_eq!(source.path.as_deref(), Some(source_path.as_path()));
    assert_eq!(source.base_dir.as_deref(), Some(fixture.path.as_path()));
}

#[test]
fn explicit_root_uses_explicit_file_source_context() {
    let fixture = TempConfigDir::new("explicit_root_source");
    let source_path = fixture.join("custom/pishoo.conf");
    let failure = parse_role_document(
        "pishoo { server { listen all 443; } }",
        &source_path,
        ConfigDocumentRole::HypervisorRoot { home: None },
    )
    .expect_err("an explicit root without home context must provide TLS paths");

    assert!(matches!(
        failure.error,
        LoadConfigError::BuildDocument { .. }
    ));
    assert!(
        failure
            .diagnostic()
            .to_string()
            .contains(&source_path.display().to_string())
    );
}

#[test]
fn identity_document_returns_detached_server_fragments() {
    let fixture = TempConfigDir::new("identity_fragments");
    let home = fixture.worker_home();
    let name = dhttp::name::DhttpName::try_from("alice.dhttp.net".to_owned())
        .expect("identity name should be valid");
    let profile = home.identity_profile(name);
    let ParsedConfigDocument::IdentityServers(servers) = parse_role_document(
        "server { listen all 443; } server { listen all 444; }",
        &fixture.join(".dhttp/identities/alice/server.conf"),
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
    let fixture = TempConfigDir::new("identity_cardinality");
    let home = fixture.worker_home();
    let name = dhttp::name::DhttpName::try_from("alice.dhttp.net".to_owned())
        .expect("identity name should be valid");
    let profile = home.identity_profile(name);
    let failure = parse_role_document(
        "",
        &fixture.join(".dhttp/identities/alice/server.conf"),
        ConfigDocumentRole::IdentityServer {
            home: &home,
            profile: &profile,
        },
    )
    .expect_err("identity server document must not be empty");

    let ConfigDocumentRoleError::MissingIdentityServer { role, span } = expect_role_error(&failure)
    else {
        panic!("expected missing identity server error");
    };
    assert_eq!(*role, ConfigDocumentRoleKind::IdentityServer);
    assert_eq!(Some(span.document_id()), failure.document_id());
}

#[test]
fn resolved_config_path_is_absolute_and_source_anchored() {
    let fixture = TempConfigDir::new("resolved_config_path");
    let include_dir = fixture.join("includes");
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
        &fixture.join("pishoo.conf"),
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
}

#[test]
fn document_ids_keep_equal_source_spans_distinct() {
    let root_fixture = TempConfigDir::new("root_document_id");
    let worker_fixture = TempConfigDir::new("worker_document_id");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let ParsedConfigDocument::HypervisorRoot(root) = parser
        .parse_text(
            "pishoo {}",
            &root_fixture.join("pishoo.conf"),
            ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect("root document should parse")
    else {
        panic!("expected hypervisor root fragment");
    };
    let home = worker_fixture.worker_home();
    let ParsedConfigDocument::WorkerPishoo(worker) = parser
        .parse_text(
            "pishoo {}",
            &worker_fixture.worker_source_path(),
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
fn role_parser_rejects_leaf_pishoo_registration_without_panicking() {
    let fixture = TempConfigDir::new("invalid_pishoo_registration");
    let mut registry = crate::parse::default_registry();
    registry.register_directive(
        context::ROOT,
        DirectiveSpec::leaf_value::<PathConfig>(
            "pishoo",
            vec![context::ROOT],
            DuplicatePolicy::Reject,
            CascadePolicy::None,
            TransportPolicy::WorkerLocalOnly,
            ReloadImpact::Supervisor,
        ),
    );
    let text = format!("pishoo {};", fixture.join("value").display());
    let failure = ConfigDocumentParser::new(&registry)
        .parse_text(
            &text,
            &fixture.join("pishoo.conf"),
            ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect_err("a role-required pishoo registration must be a pishoo context block");

    let ConfigDocumentRoleError::InvalidDirectiveRegistration {
        directive,
        role,
        expected_child_context,
        span,
    } = expect_role_error(&failure)
    else {
        panic!("expected invalid role directive registration");
    };
    assert_eq!(directive.as_str(), "pishoo");
    assert_eq!(*role, ConfigDocumentRoleKind::HypervisorRoot);
    assert_eq!(*expected_child_context, context::PISHOO);
    assert_eq!(Some(span.document_id()), failure.document_id());
}

#[test]
fn payload_free_location_registration_returns_typed_error_without_panicking() {
    let fixture = TempConfigDir::new("payload_free_location");
    let home = fixture.worker_home();
    let mut registry = crate::parse::default_registry();
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::context_empty(
            "location",
            vec![context::SERVER],
            context::LOCATION,
            DuplicatePolicy::Append,
            CascadePolicy::None,
            TransportPolicy::WorkerLocalOnly,
            ReloadImpact::RuntimeState,
        ),
    );
    let mut parser = ConfigDocumentParser::new(&registry);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parser.parse_text(
            "server { listen all 443; location { } }",
            &fixture.join("identity/server.conf"),
            ConfigDocumentRole::IdentityServer {
                home: &home,
                profile: &home.identity_profile(
                    dhttp::name::DhttpName::try_from("alice.dhttp.net".to_owned())
                        .expect("identity name"),
                ),
            },
        )
    }));

    assert!(
        outcome.is_ok(),
        "invalid public registry contracts must not panic"
    );
    assert!(
        outcome.unwrap().is_err(),
        "invalid location shape must fail"
    );
}

#[test]
fn identity_role_rejects_leaf_server_registration_without_bypassing_cardinality() {
    let fixture = TempConfigDir::new("invalid_server_registration");
    let home = fixture.worker_home();
    let name = dhttp::name::DhttpName::try_from("alice.dhttp.net".to_owned())
        .expect("identity name should be valid");
    let profile = home.identity_profile(name);
    let mut registry = crate::parse::default_registry();
    registry.register_directive(
        context::ROOT,
        DirectiveSpec::leaf_value::<PathConfig>(
            "server",
            vec![context::ROOT],
            DuplicatePolicy::Append,
            CascadePolicy::None,
            TransportPolicy::HypervisorOnly,
            ReloadImpact::ListenerSet,
        ),
    );
    let text = format!("server {};", fixture.join("value").display());
    let failure = ConfigDocumentParser::new(&registry)
        .parse_text(
            &text,
            &fixture.join(".dhttp/identities/alice/server.conf"),
            ConfigDocumentRole::IdentityServer {
                home: &home,
                profile: &profile,
            },
        )
        .expect_err("a role-required server registration must be a server context block");

    let ConfigDocumentRoleError::InvalidDirectiveRegistration {
        directive,
        role,
        expected_child_context,
        span,
    } = expect_role_error(&failure)
    else {
        panic!("expected invalid role directive registration");
    };
    assert_eq!(directive.as_str(), "server");
    assert_eq!(*role, ConfigDocumentRoleKind::IdentityServer);
    assert_eq!(*expected_child_context, context::SERVER);
    assert_eq!(Some(span.document_id()), failure.document_id());
}

#[test]
fn document_id_exhaustion_has_no_document_namespace_or_source_span() {
    let fixture = TempConfigDir::new("document_id_exhaustion");
    let registry = crate::parse::default_registry();
    let maximum_index = u64::from(u32::MAX);
    let mut parser = ConfigDocumentParser::with_next_document_index(&registry, maximum_index);
    let ParsedConfigDocument::HypervisorRoot(maximum) = parser
        .parse_text(
            "pishoo {}",
            &fixture.join("maximum.conf"),
            ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect("the maximum document id should allocate")
    else {
        panic!("expected hypervisor root fragment");
    };
    assert_eq!(
        maximum.document_id(),
        crate::parse::domain::ConfigDocumentId::try_from_index(maximum_index)
            .expect("maximum index should be supported")
    );

    let failure = parser
        .parse_text(
            "pishoo {}",
            &fixture.join("exhausted.conf"),
            ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect_err("the document id allocator should be exhausted");

    assert!(failure.document_id().is_none());
    assert!(failure.source_map.get(SourceId(0)).is_none());
    assert!(matches!(
        failure.error,
        LoadConfigError::DocumentId {
            source: crate::parse::domain::ConfigDocumentIdError::IndexOverflow { index, .. },
        } if index == maximum_index + 1
    ));
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
    assert_eq!(types.duplicate, DuplicatePolicy::Reject);
    assert_eq!(types.cascade, CascadePolicy::ReplaceWhole);
    assert_eq!(types.transport, TransportPolicy::WorkerInheritable);
    assert_eq!(types.reload, ReloadImpact::RuntimeState);

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
    assert!(
        crate::parse::domain::ConfigDocumentId::try_from_index(u64::from(u32::MAX) + 1).is_err()
    );
}

#[test]
fn loose_documents_do_not_expose_comparable_fake_namespaces() {
    let first = parse_doc("pishoo {}");
    let second = parse_doc("pishoo {}");

    assert!(first.document_id().is_none());
    assert!(second.document_id().is_none());
}

#[test]
fn missing_worker_fragment_still_builds_root_and_pishoo() {
    let fixture = TempConfigDir::new("missing_worker_fragment");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let snapshot = root_snapshot_fixture(&registry, &mut parser, &fixture);
    let home = fixture.worker_home();
    let servers = parse_identity_fragments(
        &mut parser,
        "server { listen all 443; }",
        &fixture.join("worker/identity/server.conf"),
        &home,
        "alice.dhttp.net",
    );

    let tree =
        build_worker_tree(&registry, snapshot, None, servers).expect("worker tree should seal");

    assert_eq!(
        tree.pishoo().parent_link(),
        ParentLink::Node(tree.root().id())
    );
    let server = tree
        .servers()
        .next()
        .expect("identity server should attach");
    assert_eq!(
        server.node().parent_link(),
        ParentLink::Node(tree.pishoo().id())
    );
}

#[test]
fn worker_scalar_override_wins_over_root_snapshot() {
    let fixture = TempConfigDir::new("worker_override");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let snapshot = root_snapshot_fixture(&registry, &mut parser, &fixture);
    let home = fixture.worker_home();
    let worker = parse_worker_fragment(
        &mut parser,
        "pishoo { gzip off; }",
        &fixture.worker_source_path(),
        &home,
    );
    let tree =
        build_worker_tree(&registry, snapshot, Some(worker), Vec::new()).expect("tree should seal");

    let gzip = tree
        .pishoo()
        .cascaded(GZIP)
        .expect("gzip query should succeed")
        .expect("gzip has a builtin fallback");
    assert!(!gzip.effective().0);
}

#[test]
fn worker_uses_required_snapshot_value_with_builtin_origin() {
    let registry = crate::parse::default_registry();
    let snapshot = snapshot_test_support::snapshot_with_builtin_gzip(true);

    let tree = build_worker_tree(&registry, snapshot, None, Vec::new()).expect("tree should seal");
    let gzip = tree
        .pishoo()
        .cascaded(GZIP)
        .expect("gzip query should succeed")
        .expect("complete snapshot supplies gzip");

    assert!(gzip.effective().0);
    assert_eq!(
        gzip.lineage(),
        [crate::parse::cascade::ConfigOrigin::Builtin {
            directive: GZIP.name(),
        }]
    );
}

#[test]
fn snapshot_container_queries_share_arc_and_local_override_uses_local_arc() {
    let fixture = TempConfigDir::new("snapshot_arc_sharing");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { gzip_types text/plain application/json; }",
        &fixture.join("root/pishoo.conf"),
        None,
    );
    let snapshot = build_global_tree(&registry, root, Vec::new())
        .expect("root tree")
        .root_snapshot()
        .expect("snapshot");
    let transported = snapshot_test_support::gzip_types_arc(&snapshot).expect("gzip_types");
    let snapshot_clone = snapshot.clone();
    assert!(Arc::ptr_eq(
        &transported,
        &snapshot_test_support::gzip_types_arc(&snapshot_clone).expect("cloned gzip_types")
    ));

    let inherited_tree =
        build_worker_tree(&registry, snapshot_clone, None, Vec::new()).expect("worker tree");
    let first = inherited_tree
        .pishoo()
        .cascaded(GZIP_TYPES)
        .expect("query")
        .expect("gzip_types");
    let second = inherited_tree
        .pishoo()
        .cascaded(GZIP_TYPES)
        .expect("query")
        .expect("gzip_types");
    assert!(Arc::ptr_eq(first.effective(), second.effective()));
    assert!(Arc::ptr_eq(first.effective(), &transported));

    let home = fixture.worker_home();
    let worker = parse_worker_fragment(
        &mut parser,
        "pishoo { gzip_types image/png; }",
        &fixture.worker_source_path(),
        &home,
    );
    let local_tree = build_worker_tree(&registry, snapshot, Some(worker), Vec::new())
        .expect("local worker tree");
    let local = local_tree
        .pishoo()
        .cascaded(GZIP_TYPES)
        .expect("query")
        .expect("local gzip_types");
    assert!(!Arc::ptr_eq(local.effective(), &transported));
}

#[test]
fn types_replaces_the_whole_parent_map() {
    let fixture = TempConfigDir::new("types_replace");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let snapshot = root_snapshot_fixture(&registry, &mut parser, &fixture);
    let home = fixture.worker_home();
    let servers = parse_identity_fragments(
        &mut parser,
        "server { listen all 443; types { text/server server; } }",
        &fixture.join("worker/identity/server.conf"),
        &home,
        "alice.dhttp.net",
    );
    let tree = build_worker_tree(&registry, snapshot, None, servers).expect("tree should seal");

    let types = tree
        .servers()
        .next()
        .expect("server should attach")
        .node()
        .cascaded(TYPES)
        .expect("types query should succeed")
        .expect("types should be configured");
    assert_eq!(types.effective().0.len(), 1);
    assert!(types.effective().0.contains_key("server"));
    assert!(!types.effective().0.contains_key("root"));
}

#[test]
fn cascade_query_rejects_registry_policy_mismatch() {
    let fixture = TempConfigDir::new("cascade_policy_mismatch");
    let mut registry = crate::parse::default_registry();
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::raw_value::<MimeTypes>(
            "types",
            vec![context::SERVER],
            DuplicatePolicy::Reject,
            CascadePolicy::NearestWins,
            TransportPolicy::WorkerLocalOnly,
            ReloadImpact::RuntimeState,
        ),
    );
    let mut parser = ConfigDocumentParser::new(&registry);
    let snapshot = root_snapshot_fixture(&registry, &mut parser, &fixture);
    let home = fixture.worker_home();
    let servers = parse_identity_fragments(
        &mut parser,
        "server { listen all 443; types { text/server server; } }",
        &fixture.join("worker/identity/server.conf"),
        &home,
        "alice.dhttp.net",
    );
    let tree = build_worker_tree(&registry, snapshot, None, servers).expect("tree should seal");
    let server = tree.servers().next().expect("server should attach");

    assert!(
        server.node().cascaded(TYPES).is_err(),
        "cascade policy mismatches must not use DirectiveKey hardcoding"
    );
}

#[test]
fn snapshot_contract_rejects_worker_local_or_wrong_domain_registration() {
    let fixture = TempConfigDir::new("snapshot_registry_contract");

    let mut worker_local_registry = crate::parse::default_registry();
    worker_local_registry.register_directive(
        context::PISHOO,
        DirectiveSpec::leaf_value::<BoolConfig>(
            "gzip",
            vec![context::PISHOO],
            DuplicatePolicy::Reject,
            CascadePolicy::NearestWins,
            TransportPolicy::WorkerLocalOnly,
            ReloadImpact::RuntimeState,
        ),
    );
    let mut parser = ConfigDocumentParser::new(&worker_local_registry);
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { gzip on; }",
        &fixture.join("worker-local/pishoo.conf"),
        None,
    );
    assert!(
        build_global_tree(&worker_local_registry, root, Vec::new()).is_err(),
        "WorkerLocalOnly values must not enter the V1 snapshot contract"
    );
    assert!(
        build_worker_tree(
            &worker_local_registry,
            snapshot_test_support::snapshot_with_builtin_gzip(true),
            None,
            Vec::new(),
        )
        .is_err(),
        "worker overlays must validate the same registry contract before applying a snapshot"
    );

    let mut wrong_domain_registry = crate::parse::default_registry();
    wrong_domain_registry.register_directive(
        context::PISHOO,
        DirectiveSpec::leaf_value::<StringList>(
            "gzip",
            vec![context::PISHOO],
            DuplicatePolicy::Reject,
            CascadePolicy::NearestWins,
            TransportPolicy::WorkerInheritable,
            ReloadImpact::RuntimeState,
        ),
    );
    let mut parser = ConfigDocumentParser::new(&wrong_domain_registry);
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { gzip on; }",
        &fixture.join("wrong-domain/pishoo.conf"),
        None,
    );
    assert!(
        build_global_tree(&wrong_domain_registry, root, Vec::new()).is_err(),
        "the V1 gzip field must remain bound to BoolConfig"
    );

    let mut premature_access_log_registry = crate::parse::default_registry();
    premature_access_log_registry.register_directive(
        context::PISHOO,
        DirectiveSpec::leaf_value::<StringList>(
            "access_log",
            vec![context::PISHOO],
            DuplicatePolicy::Reject,
            CascadePolicy::NearestWins,
            TransportPolicy::WorkerLocalOnly,
            ReloadImpact::RuntimeState,
        ),
    );
    let mut parser = ConfigDocumentParser::new(&premature_access_log_registry);
    let root = parse_root_fragment(
        &mut parser,
        "pishoo {}",
        &fixture.join("reserved-access-log/pishoo.conf"),
        None,
    );
    assert!(
        build_global_tree(&premature_access_log_registry, root, Vec::new()).is_err(),
        "the reserved V1 access_log slot must remain absent until its checked domain is registered"
    );
}

#[test]
fn snapshot_contract_rejects_extra_worker_inheritable_directive() {
    let fixture = TempConfigDir::new("snapshot_registry_extra");
    let mut registry = crate::parse::default_registry();
    registry.register_directive(
        context::PISHOO,
        DirectiveSpec::leaf_value::<BoolConfig>(
            "extra_inherited",
            vec![context::PISHOO],
            DuplicatePolicy::Reject,
            CascadePolicy::NearestWins,
            TransportPolicy::WorkerInheritable,
            ReloadImpact::RuntimeState,
        ),
    );
    let mut parser = ConfigDocumentParser::new(&registry);
    let root = parse_root_fragment(
        &mut parser,
        "pishoo {}",
        &fixture.join("root/pishoo.conf"),
        None,
    );

    assert!(matches!(
        build_global_tree(&registry, root, Vec::new()),
        Err(crate::parse::tree::HomeConfigTreeError::SnapshotContract {
            source:
                crate::parse::registry::V1SnapshotSchemaError::ExtraWorkerInheritableDirective {
                    directive,
                },
        }) if directive.as_str() == "extra_inherited"
    ));
    assert!(matches!(
        build_worker_tree(
            &registry,
            snapshot_test_support::snapshot_with_builtin_gzip(true),
            None,
            Vec::new(),
        ),
        Err(crate::parse::tree::HomeConfigTreeError::SnapshotContract {
            source:
                crate::parse::registry::V1SnapshotSchemaError::ExtraWorkerInheritableDirective {
                    directive,
                },
        }) if directive.as_str() == "extra_inherited"
    ));
}

#[test]
fn cascade_lineage_is_builtin_root_worker_server_location() {
    let fixture = TempConfigDir::new("cascade_lineage");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let snapshot = root_snapshot_fixture(&registry, &mut parser, &fixture);
    let home = fixture.worker_home();
    let worker = parse_worker_fragment(
        &mut parser,
        "pishoo { gzip off; }",
        &fixture.worker_source_path(),
        &home,
    );
    let servers = parse_identity_fragments(
        &mut parser,
        "server { listen all 443; gzip on; location / { gzip off; } }",
        &fixture.join("worker/identity/server.conf"),
        &home,
        "alice.dhttp.net",
    );
    let tree =
        build_worker_tree(&registry, snapshot, Some(worker), servers).expect("tree should seal");
    let location = tree
        .servers()
        .next()
        .expect("server")
        .locations()
        .next()
        .expect("location");
    let gzip = location
        .node()
        .cascaded(GZIP)
        .expect("gzip query")
        .expect("gzip fallback");

    assert!(!gzip.effective().0);
    assert_eq!(gzip.lineage().len(), 5);
    assert!(matches!(
        gzip.lineage()[0],
        crate::parse::cascade::ConfigOrigin::Builtin { .. }
    ));
    assert!(matches!(
        gzip.lineage()[1],
        crate::parse::cascade::ConfigOrigin::RootInherited { .. }
    ));
    assert!(
        gzip.lineage()[2..]
            .iter()
            .all(|origin| matches!(origin, crate::parse::cascade::ConfigOrigin::Source(_)))
    );
}

#[test]
fn identity_server_parent_is_home_pishoo() {
    let fixture = TempConfigDir::new("identity_parent");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let snapshot = root_snapshot_fixture(&registry, &mut parser, &fixture);
    let home = fixture.worker_home();
    let servers = parse_identity_fragments(
        &mut parser,
        "server { listen all 443; }",
        &fixture.join("identity/server.conf"),
        &home,
        "alice.dhttp.net",
    );
    let tree = build_worker_tree(&registry, snapshot, None, servers).expect("tree should seal");
    let server = tree.servers().next().expect("server");

    assert_eq!(
        server.node().parent_link(),
        ParentLink::Node(tree.pishoo().id())
    );
}

#[test]
fn global_direct_server_parent_is_global_pishoo() {
    let fixture = TempConfigDir::new("global_server_parent");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = DhttpHome::new(fixture.join("global-home"));
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { server { listen all 443; } }",
        &fixture.join("global-home/pishoo.conf"),
        Some(&home),
    );
    let tree = build_global_tree(&registry, root, Vec::new()).expect("global tree should seal");
    let server = tree.servers().next().expect("server");

    assert_eq!(
        server.node().parent_link(),
        ParentLink::Node(tree.pishoo().id())
    );
}

#[test]
fn global_identity_server_parent_is_global_pishoo() {
    let fixture = TempConfigDir::new("global_identity_parent");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = DhttpHome::new(fixture.join("global-home"));
    let root = parse_root_fragment(
        &mut parser,
        "pishoo {}",
        &fixture.join("global-home/pishoo.conf"),
        Some(&home),
    );
    let identities = parse_identity_fragments(
        &mut parser,
        "server { listen all 443; }",
        &fixture.join("global-home/identities/alice/server.conf"),
        &home,
        "alice.dhttp.net",
    );

    let tree = build_global_tree(&registry, root, identities).expect("global tree should seal");
    let server = tree.servers().next().expect("global identity server");

    assert_eq!(
        server.node().parent_link(),
        ParentLink::Node(tree.pishoo().id())
    );
}

#[test]
fn attached_registry_finalizer_observes_parent_and_complete_siblings() {
    let fixture = TempConfigDir::new("attached_finalizer");
    let mut registry = crate::parse::default_registry();
    registry.register_context(ContextSpec {
        key: context::SERVER,
        finalize: Some(count_local_server_finalizer),
    });
    registry.register_attached_finalizer(context::SERVER, assert_attached_server_context);
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = DhttpHome::new(fixture.join("global-home"));
    LOCAL_FINALIZER_CALLS.store(0, Ordering::Relaxed);
    ATTACHED_FINALIZER_CALLS.store(0, Ordering::Relaxed);
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { server { listen all 443; } server { listen all 444; } }",
        &fixture.join("global-home/pishoo.conf"),
        Some(&home),
    );

    assert_eq!(LOCAL_FINALIZER_CALLS.load(Ordering::Relaxed), 2);
    assert_eq!(ATTACHED_FINALIZER_CALLS.load(Ordering::Relaxed), 0);
    let tree = build_global_tree(&registry, root, Vec::new()).expect("global tree should seal");

    assert_eq!(tree.servers().count(), 2);
    assert_eq!(LOCAL_FINALIZER_CALLS.load(Ordering::Relaxed), 2);
    assert_eq!(ATTACHED_FINALIZER_CALLS.load(Ordering::Relaxed), 2);
}

#[test]
fn location_parent_is_its_server() {
    let fixture = TempConfigDir::new("location_parent");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = DhttpHome::new(fixture.join("global-home"));
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { server { listen all 443; location / { root /tmp; } } }",
        &fixture.join("global-home/pishoo.conf"),
        Some(&home),
    );
    let tree = build_global_tree(&registry, root, Vec::new()).expect("global tree should seal");
    let server = tree.servers().next().expect("server");
    let location = server.locations().next().expect("location");

    assert_eq!(
        location.node().parent_link(),
        ParentLink::Node(server.node().id())
    );
}

#[test]
fn tree_ref_keeps_source_and_parent_alive() {
    let fixture = TempConfigDir::new("ref_lifetime");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = DhttpHome::new(fixture.join("global-home"));
    let source_path = fixture.join("global-home/pishoo.conf");
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { server { listen all 443; } }",
        &source_path,
        Some(&home),
    );
    let tree = build_global_tree(&registry, root, Vec::new()).expect("tree should seal");
    let server = tree.servers().next().expect("server");
    let span = server.node().source_span().expect("source span");
    drop(tree);

    assert_eq!(
        server.node().parent_link(),
        ParentLink::Node(server.tree().pishoo().id())
    );
    assert_eq!(server.tree().source_path(span), Some(source_path.as_path()));
}

#[test]
fn sealed_source_bundle_retains_only_one_source_map_owner() {
    let fixture = TempConfigDir::new("source_owner_weight");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = DhttpHome::new(fixture.join("global-home"));
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { server { listen all 443; location / {} } }",
        &fixture.join("global-home/pishoo.conf"),
        Some(&home),
    );
    let source_owner = root.source_owner();

    let tree = build_global_tree(&registry, root, Vec::new()).expect("tree should seal");

    assert_eq!(tree.servers().count(), 1);
    assert_eq!(Arc::strong_count(&source_owner), 2);
}

#[test]
fn cross_document_diagnostics_keep_the_correct_source_bundle() {
    let fixture = TempConfigDir::new("cross_document_sources");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let snapshot = root_snapshot_fixture(&registry, &mut parser, &fixture);
    let home = fixture.worker_home();
    let alice_path = fixture.join("alice/server.conf");
    let bob_path = fixture.join("bob/server.conf");
    let mut alice_parser = ConfigDocumentParser::new(&registry);
    let mut servers = parse_identity_fragments(
        &mut alice_parser,
        "server { listen all 443; }",
        &alice_path,
        &home,
        "alice.dhttp.net",
    );
    let mut bob_parser = ConfigDocumentParser::new(&registry);
    servers.extend(parse_identity_fragments(
        &mut bob_parser,
        "server { listen all 444; }",
        &bob_path,
        &home,
        "bob.dhttp.net",
    ));
    let tree = build_worker_tree(&registry, snapshot, None, servers).expect("tree should seal");
    let paths = tree
        .servers()
        .map(|server| {
            let span = server.node().source_span().expect("source span");
            tree.source_path(span).expect("source path").to_path_buf()
        })
        .collect::<Vec<_>>();

    assert_eq!(paths, vec![alice_path, bob_path]);
}

#[test]
fn root_snapshot_contains_only_worker_inheritable_values() {
    let fixture = TempConfigDir::new("snapshot_allowlist");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = DhttpHome::new(fixture.join("global-home"));
    let root = parse_root_fragment(
        &mut parser,
        "pishoo { pid /run/pishoo.pid; workers alice; groups www; gzip on; server { listen all 443; } }",
        &fixture.join("global-home/pishoo.conf"),
        Some(&home),
    );
    let snapshot = build_global_tree(&registry, root, Vec::new())
        .expect("root tree")
        .root_snapshot()
        .expect("snapshot");

    assert_eq!(
        snapshot_test_support::wire_field_names(&snapshot),
        [
            "access_rules",
            "gzip",
            "gzip_vary",
            "gzip_min_length",
            "gzip_comp_level",
            "gzip_types",
            "default_type",
            "types",
            "access_log",
        ]
    );
    assert!(snapshot.gzip().0);
}

#[test]
fn root_direct_servers_are_not_serialized() {
    let fixture = TempConfigDir::new("snapshot_no_servers");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = DhttpHome::new(fixture.join("global-home"));
    let without_server = parse_root_fragment(
        &mut parser,
        "pishoo { gzip on; }",
        &fixture.join("without/pishoo.conf"),
        Some(&home),
    );
    let with_server = parse_root_fragment(
        &mut parser,
        "pishoo { gzip on; server { listen all 443; } }",
        &fixture.join("with/pishoo.conf"),
        Some(&home),
    );
    let first = build_global_tree(&registry, without_server, Vec::new())
        .unwrap()
        .root_snapshot()
        .unwrap();
    let second = build_global_tree(&registry, with_server, Vec::new())
        .unwrap()
        .root_snapshot()
        .unwrap();

    assert_ne!(first, second, "source origin remains part of equality");
    assert_eq!(
        snapshot_test_support::values_without_origins(&first),
        snapshot_test_support::values_without_origins(&second)
    );
}

#[test]
fn resolved_root_path_round_trips_without_worker_rebase() {
    let root = ResolvedConfigPath::try_from(PathBuf::from("/srv/root/logs/access.log")).unwrap();
    let decoded = snapshot_test_support::round_trip_resolved_path(root.clone()).unwrap();

    assert_eq!(decoded, root);
}

#[cfg(unix)]
#[test]
fn resolved_root_non_utf8_path_round_trips_on_unix() {
    use std::os::unix::ffi::OsStringExt;

    let path = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/non-utf8-\xff".to_vec()));
    let resolved = ResolvedConfigPath::try_from(path).unwrap();
    let decoded = snapshot_test_support::round_trip_resolved_path(resolved.clone()).unwrap();

    assert_eq!(decoded, resolved);
}

#[test]
fn snapshot_absolute_path_rejects_relative_or_nul_bytes() {
    assert!(snapshot_test_support::decode_absolute_path(b"relative/path").is_err());
    assert!(snapshot_test_support::decode_absolute_path(b"/tmp/nul\0path").is_err());
}

#[test]
fn root_path_without_source_base_is_a_configuration_error() {
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let failure = parser
        .parse_text(
            "pishoo { access_rules sqlite:relative/rules.db; }",
            Path::new("pishoo.conf"),
            ConfigDocumentRole::HypervisorRoot { home: None },
        )
        .expect_err("unanchored root path must be rejected");
    let error = snafu::Report::from_error(&failure.error).to_string();

    assert!(error.contains("resolve relative access_rules sqlite path"));
}

#[test]
fn unknown_snapshot_schema_is_rejected() {
    assert!(snapshot_test_support::decode_schema(2).is_err());
}

#[test]
fn snapshot_real_codec_round_trips_and_rejects_unknown_or_malformed_wire() {
    let snapshot = snapshot_test_support::snapshot_with_builtin_gzip(true);

    assert_eq!(
        snapshot_test_support::codec_round_trip(&snapshot).expect("real codec round trip"),
        snapshot
    );
    assert!(snapshot_test_support::codec_rejects_unknown_schema(
        &snapshot
    ));
    assert!(snapshot_test_support::codec_rejects_malformed_access_rules(
        &snapshot
    ));
}

#[test]
fn snapshot_access_rules_revalidates_url_domain() {
    assert!(matches!(
        snapshot_test_support::decode_access_rules("not a URL"),
        Err(
            crate::parse::snapshot::RootConfigSnapshotError::AccessRulesUrl {
                source: url::ParseError::RelativeUrlWithoutBase,
            }
        )
    ));
    assert!(snapshot_test_support::decode_access_rules("https://example.com/rules.db").is_err());
    assert!(snapshot_test_support::decode_access_rules("sqlite://host/tmp/rules.db").is_err());
}

#[test]
fn access_rules_rejects_literal_and_percent_encoded_nul_in_text_and_snapshot() {
    for uri in ["sqlite:/tmp/rules\0.db", "sqlite:/tmp/rules%00.db"] {
        let failure =
            crate::parse::parse_config_str_for_test(&format!("pishoo {{ access_rules {uri}; }}"))
                .expect_err("text access_rules path containing NUL must fail");
        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("NUL byte")
        );

        let error = snapshot_test_support::decode_access_rules(uri)
            .expect_err("snapshot access_rules path containing NUL must fail");
        assert!(matches!(
            error,
            crate::parse::snapshot::RootConfigSnapshotError::AccessRules {
                source: crate::parse::types::AccessRulesUriValidationError::NulPath,
            }
        ));
    }
}

#[test]
fn snapshot_header_values_revalidate_from_bytes() {
    assert!(snapshot_test_support::decode_header_value(b"text/plain").is_ok());
    assert!(snapshot_test_support::decode_header_value(b"bad\nvalue").is_err());
}

#[test]
fn snapshot_mime_entries_are_sorted_and_reject_duplicates() {
    let types = MimeTypes(std::collections::HashMap::from([
        ("z".to_owned(), http::HeaderValue::from_static("text/z")),
        ("a".to_owned(), http::HeaderValue::from_static("text/a")),
    ]));
    assert_eq!(
        snapshot_test_support::mime_wire_extensions(&types),
        ["a", "z"]
    );
    assert!(
        snapshot_test_support::decode_mime_entries(&[("a", b"text/a"), ("a", b"text/b")]).is_err()
    );
    assert!(snapshot_test_support::decode_mime_entries(&[("", b"text/a")]).is_err());
}

#[test]
fn text_and_snapshot_mime_decode_use_the_same_validation_category() {
    let text_failure =
        crate::parse::parse_config_str_for_test("pishoo { types { text/a a; 'bad\nvalue' a; } }")
            .expect_err("text MIME value containing a newline must fail");
    let mut text_error: &(dyn std::error::Error + 'static) = &text_failure.error;
    let text_category = loop {
        if let Some(category) =
            text_error.downcast_ref::<crate::parse::types::MimeTypesValidationError>()
        {
            break category;
        }
        text_error = text_error
            .source()
            .expect("text MIME failure should retain the domain error");
    };
    assert!(matches!(
        text_category,
        crate::parse::types::MimeTypesValidationError::HeaderValue { .. }
    ));

    assert!(matches!(
        snapshot_test_support::decode_mime_entries(&[("a", b"text/a"), ("a", b"bad\nvalue")]),
        Err(crate::parse::snapshot::RootConfigSnapshotError::MimeTypes {
            source: crate::parse::types::MimeTypesValidationError::HeaderValue { .. },
        })
    ));
}

#[test]
fn snapshot_preserves_absence_and_gzip_fallbacks() {
    let fixture = TempConfigDir::new("snapshot_fallbacks");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let root = parse_root_fragment(
        &mut parser,
        "pishoo {}",
        &fixture.join("root/pishoo.conf"),
        None,
    );
    let snapshot = build_global_tree(&registry, root, Vec::new())
        .unwrap()
        .root_snapshot()
        .unwrap();

    assert!(snapshot.access_rules().is_none());
    assert!(!snapshot.gzip().0);
    assert!(!snapshot.gzip_vary().0);
    assert_eq!(snapshot.gzip_min_length().0, 20);
    assert_eq!(snapshot.gzip_comp_level().0, 1);
    assert!(snapshot.gzip_types().is_none());
    assert!(snapshot.default_type().is_none());
    assert!(snapshot.types().is_none());
    assert!(!snapshot_test_support::has_access_log(&snapshot));
}

#[test]
fn snapshot_equality_includes_origin() {
    let fixture = TempConfigDir::new("snapshot_origin_equality");
    let registry = crate::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let first = parse_root_fragment(
        &mut parser,
        "pishoo { gzip on; }",
        &fixture.join("one/pishoo.conf"),
        None,
    );
    let second = parse_root_fragment(
        &mut parser,
        "pishoo {\n gzip on;\n}",
        &fixture.join("two/pishoo.conf"),
        None,
    );
    let first = build_global_tree(&registry, first, Vec::new())
        .unwrap()
        .root_snapshot()
        .unwrap();
    let second = build_global_tree(&registry, second, Vec::new())
        .unwrap()
        .root_snapshot()
        .unwrap();

    assert_eq!(
        snapshot_test_support::checked_wire_round_trip(&first).unwrap(),
        first
    );
    assert_ne!(first, second);
    assert_eq!(
        snapshot_test_support::values_without_origins(&first),
        snapshot_test_support::values_without_origins(&second)
    );
}

use snafu::{OptionExt, Snafu, ensure};

use crate::parse::{
    document::ConfigNode,
    domain::ResolvedConfigPath,
    registry::{
        BuildOptions, CascadePolicy, ConfigRegistry, ContextKey, DirectiveSpec, DuplicatePolicy,
        LocalDirectiveKey, ReloadImpact, RepeatedCardinality, RepeatedDirectiveKey,
        SingleCardinality, TransportPolicy, TypedDirectiveDefinition, context,
    },
    source::SourceSpan,
    tree::AttachedConfigNode,
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, ListenConfig,
        MimeTypes, ResolverConfig, ServerName, ServerNames, StringList,
    },
    value::TypedValue,
};

macro_rules! single_definition {
    ($definition:ident, $key:ident, $value:ty, $name:literal, $reload:expr) => {
        const $definition: TypedDirectiveDefinition<$value, SingleCardinality> =
            TypedDirectiveDefinition::single_leaf(
                context::SERVER,
                $name,
                DuplicatePolicy::Reject,
                CascadePolicy::NearestWins,
                TransportPolicy::WorkerLocalOnly,
                $reload,
            );
        pub(crate) const $key: LocalDirectiveKey<$value> = $definition.key();
    };
}

const LISTEN_DEFINITION: TypedDirectiveDefinition<ListenConfig, RepeatedCardinality> =
    TypedDirectiveDefinition::repeated_leaf(
        context::SERVER,
        "listen",
        CascadePolicy::NearestWins,
        TransportPolicy::WorkerLocalOnly,
        ReloadImpact::ListenerSet,
    );
pub(crate) const LISTEN_KEY: RepeatedDirectiveKey<ListenConfig> = LISTEN_DEFINITION.key();
single_definition!(
    SERVER_NAME_DEFINITION,
    SERVER_NAME_KEY,
    ServerNames,
    "server_name",
    ReloadImpact::ListenerSet
);
single_definition!(
    DNS_DEFINITION,
    DNS_KEY,
    ResolverConfig,
    "dns",
    ReloadImpact::ListenerSet
);
single_definition!(
    GZIP_DEFINITION,
    GZIP_KEY,
    BoolConfig,
    "gzip",
    ReloadImpact::RuntimeState
);
single_definition!(
    GZIP_VARY_DEFINITION,
    GZIP_VARY_KEY,
    BoolConfig,
    "gzip_vary",
    ReloadImpact::RuntimeState
);
single_definition!(
    GZIP_MIN_LENGTH_DEFINITION,
    GZIP_MIN_LENGTH_KEY,
    GzipMinLength,
    "gzip_min_length",
    ReloadImpact::RuntimeState
);
single_definition!(
    GZIP_COMP_LEVEL_DEFINITION,
    GZIP_COMP_LEVEL_KEY,
    GzipCompLevel,
    "gzip_comp_level",
    ReloadImpact::RuntimeState
);
single_definition!(
    GZIP_TYPES_DEFINITION,
    GZIP_TYPES_KEY,
    StringList,
    "gzip_types",
    ReloadImpact::RuntimeState
);
single_definition!(
    SSL_CERTIFICATE_DEFINITION,
    SSL_CERTIFICATE_KEY,
    ResolvedConfigPath,
    "ssl_certificate",
    ReloadImpact::ListenerSet
);
single_definition!(
    SSL_CERTIFICATE_KEY_DEFINITION,
    SSL_CERTIFICATE_KEY_KEY,
    ResolvedConfigPath,
    "ssl_certificate_key",
    ReloadImpact::ListenerSet
);
single_definition!(
    DEFAULT_TYPE_DEFINITION,
    DEFAULT_TYPE_KEY,
    DefaultType,
    "default_type",
    ReloadImpact::RuntimeState
);
single_definition!(
    ACCESS_RULES_DEFINITION,
    ACCESS_RULES_KEY,
    AccessRulesUri,
    "access_rules",
    ReloadImpact::RuntimeState
);
single_definition!(
    RELAY_DEFINITION,
    RELAY_KEY,
    BoolConfig,
    "relay",
    ReloadImpact::RuntimeState
);
single_definition!(
    STUN_DEFINITION,
    STUN_KEY,
    BoolConfig,
    "stun",
    ReloadImpact::RuntimeState
);
const TYPES_DEFINITION: TypedDirectiveDefinition<MimeTypes, SingleCardinality> =
    TypedDirectiveDefinition::raw(
        context::SERVER,
        "types",
        DuplicatePolicy::Reject,
        CascadePolicy::ReplaceWhole,
        TransportPolicy::WorkerLocalOnly,
        ReloadImpact::RuntimeState,
    );
pub(crate) const TYPES_KEY: LocalDirectiveKey<MimeTypes> = TYPES_DEFINITION.key();

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum FinalizeServerError {
    #[snafu(display("missing listen directive in server context"))]
    MissingListen { span: SourceSpan },
    #[snafu(display("missing ssl_certificate directive in server context"))]
    MissingCertificate { span: SourceSpan },
    #[snafu(display("missing ssl_certificate_key directive in server context"))]
    MissingCertificateKey { span: SourceSpan },
    #[snafu(display("server context is not attached to the home PISHOO context"))]
    InvalidAttachedParent { span: SourceSpan },
    #[snafu(display("attached location context is missing its parsed pattern"))]
    MissingAttachedLocationPattern { span: SourceSpan },
}

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::SERVER,
        finalize: Some(finalize_server),
    });
    registry.register_attached_finalizer(context::SERVER, finalize_attached_server);
    registry.register_directive(context::ROOT, server_block(context::ROOT));
    registry.register_directive(context::PISHOO, server_block(context::PISHOO));
    LISTEN_DEFINITION.register(registry);
    SERVER_NAME_DEFINITION.register(registry);
    DNS_DEFINITION.register(registry);
    GZIP_DEFINITION.register(registry);
    GZIP_VARY_DEFINITION.register(registry);
    GZIP_MIN_LENGTH_DEFINITION.register(registry);
    GZIP_COMP_LEVEL_DEFINITION.register(registry);
    GZIP_TYPES_DEFINITION.register(registry);
    SSL_CERTIFICATE_DEFINITION.register(registry);
    SSL_CERTIFICATE_KEY_DEFINITION.register(registry);
    DEFAULT_TYPE_DEFINITION.register(registry);
    ACCESS_RULES_DEFINITION.register(registry);
    RELAY_DEFINITION.register(registry);
    STUN_DEFINITION.register(registry);
    TYPES_DEFINITION.register(registry);
}

fn server_block(parent: ContextKey) -> DirectiveSpec {
    DirectiveSpec::context_empty(
        "server",
        vec![parent],
        context::SERVER,
        DuplicatePolicy::Append,
        CascadePolicy::None,
        TransportPolicy::HypervisorOnly,
        ReloadImpact::ListenerSet,
    )
}

fn finalize_server(
    node: &mut ConfigNode,
    options: &BuildOptions<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ensure!(
        !node.get_all::<ListenConfig>("listen")?.is_empty(),
        finalize_server_error::MissingListenSnafu { span: node.span }
    );
    if let Some(identity_profile) = options.identity_profile
        && node.get::<ServerNames>("server_name")?.is_none()
    {
        node.insert_slot(
            "server_name",
            TypedValue::new(
                ServerNames(vec![ServerName {
                    name: identity_profile.name().to_owned(),
                }]),
                node.span,
            ),
        );
    }

    let has_cert = node.get::<ResolvedConfigPath>("ssl_certificate")?.is_some();
    let has_key = node
        .get::<ResolvedConfigPath>("ssl_certificate_key")?
        .is_some();
    match (has_cert, has_key, options.has_dhttp_home_context()) {
        (true, true, _) => Ok(()),
        (false, false, true) => Ok(()),
        (false, _, _) => {
            node.get::<ResolvedConfigPath>("ssl_certificate")?
                .context(finalize_server_error::MissingCertificateSnafu { span: node.span })?;
            Ok(())
        }
        (_, false, _) => {
            node.get::<ResolvedConfigPath>("ssl_certificate_key")?
                .context(finalize_server_error::MissingCertificateKeySnafu { span: node.span })?;
            Ok(())
        }
    }
}

fn finalize_attached_server(
    node: AttachedConfigNode<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ensure!(
        node.parent()
            .is_some_and(|parent| parent.context() == context::PISHOO),
        finalize_server_error::InvalidAttachedParentSnafu {
            span: node.config().span,
        }
    );
    for location in node.children() {
        location
            .config()
            .payload::<crate::parse::pattern::Pattern>()?
            .context(finalize_server_error::MissingAttachedLocationPatternSnafu {
                span: location.config().span,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::parse::{
        domain::ResolvedConfigPath,
        tests::{
            assert_error_chain_display_single_line, build_server_conf, cleanup_temp_files,
            create_temp_file, first_server, parse_doc,
        },
        types::{
            AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, ListenConfig, MimeTypes,
            ResolverConfig, ServerNames,
        },
    };

    #[test]
    fn parse_server_rejects_legacy_server_id_directive() {
        let cert = create_temp_file("legacy_server_id_cert");
        let key = create_temp_file("legacy_server_id_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; server_id 1; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let error = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("legacy server_id directive must be rejected");

        let report = snafu::Report::from_error(&error).to_string();
        assert!(
            report.contains("server_id"),
            "error report should name the rejected directive: {report}"
        );
        assert_error_chain_display_single_line(&error);
        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_bool_directives_accept_on_off_values() {
        let cert = create_temp_file("bool_directive_cert");
        let key = create_temp_file("bool_directive_key");
        let conf = build_server_conf(&cert, &key, "gzip on; gzip_vary off; relay on; stun off;");

        let server = first_server(&parse_doc(&conf));

        assert!(server.require::<BoolConfig>("gzip").unwrap().0);
        assert!(!server.require::<BoolConfig>("gzip_vary").unwrap().0);
        assert!(server.require::<BoolConfig>("relay").unwrap().0);
        assert!(!server.require::<BoolConfig>("stun").unwrap().0);

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_server_path_directives_preserve_path_values() {
        let cert = create_temp_file("server_path_cert");
        let key = create_temp_file("server_path_key");
        let conf = build_server_conf(&cert, &key, "");

        let server = first_server(&parse_doc(&conf));

        assert_eq!(
            server
                .require::<ResolvedConfigPath>("ssl_certificate")
                .expect("ssl_certificate should be typed")
                .as_ref()
                .as_ref(),
            cert.clone()
        );
        assert_eq!(
            server
                .require::<ResolvedConfigPath>("ssl_certificate_key")
                .expect("ssl_certificate_key should be typed")
                .as_ref()
                .as_ref(),
            key.clone()
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("pishoo-{label}-{nanos}"))
    }

    fn first_root_server(
        document: &crate::parse::document::ConfigDocument,
    ) -> std::sync::Arc<crate::parse::document::ConfigNode> {
        document.root.children("server").expect("server children")[0].clone()
    }

    #[test]
    fn identity_profile_server_conf_can_omit_name_and_tls_with_home_context() {
        let home_path = unique_test_dir("identity-profile-server-conf");
        let home = dhttp::home::DhttpHome::new(home_path);
        let name = dhttp::name::DhttpName::try_from("alice.dhttp.net".to_owned()).unwrap();
        let profile = home.identity_profile(name.clone());
        std::fs::create_dir_all(profile.ssl_dir()).expect("create ssl dir");

        let document = crate::parse::load_config_text(
            "server { listen all 443; location / { root .; } }",
            Some(profile.path()),
            &crate::parse::default_registry(),
            crate::parse::registry::BuildOptions {
                dhttp_home: Some(&home),
                identity_profile: Some(&profile),
            },
        )
        .expect("identity service config should parse");

        let server = first_root_server(&document);
        let names = server.require::<ServerNames>("server_name").unwrap();
        assert_eq!(names.0[0].name, name);
        assert!(
            server
                .get::<ResolvedConfigPath>("ssl_certificate")
                .unwrap()
                .is_none()
        );
        assert!(
            server
                .get::<ResolvedConfigPath>("ssl_certificate_key")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_config_still_requires_tls_paths() {
        let failure = crate::parse::parse_config_str_for_test(
            "pishoo { server { listen all 443; server_name alice.dhttp.net; } }",
        )
        .expect_err("standalone config must reject missing tls paths");

        let report = snafu::Report::from_error(&failure.error).to_string();
        assert!(report.contains("missing ssl_certificate directive in server context"));
    }

    #[test]
    fn parse_server_accepts_default_type_and_access_rules() {
        let cert = create_temp_file("server_ctx_access_cert");
        let key = create_temp_file("server_ctx_access_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "default_type application/octet-stream; access_rules sqlite:///tmp/access.db;",
        );

        let server = first_server(&parse_doc(&conf));

        assert_eq!(
            server
                .require::<DefaultType>("default_type")
                .expect("default_type should be typed")
                .0
                .to_str()
                .unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            server
                .require::<AccessRulesUri>("access_rules")
                .expect("access_rules should be typed")
                .0
                .as_str(),
            "sqlite:///tmp/access.db"
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_server_accepts_gzip_comp_level() {
        let cert = create_temp_file("server_gzip_comp_level_cert");
        let key = create_temp_file("server_gzip_comp_level_key");
        let conf = build_server_conf(&cert, &key, "gzip_comp_level 6;");

        let server = first_server(&parse_doc(&conf));

        assert_eq!(
            server
                .require::<GzipCompLevel>("gzip_comp_level")
                .expect("gzip_comp_level should be typed")
                .0,
            6
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_resolver_config_rejects_deprecated_kind() {
        let cert = create_temp_file("resolver_deprecated_cert");
        let key = create_temp_file("resolver_deprecated_key");
        let conf = build_server_conf(&cert, &key, "dns udp https://dns.example.com/dns-query;");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("deprecated dns resolver kind should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("deprecated resolver kind `udp`")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_resolver_config_rejects_unsupported_kind() {
        let cert = create_temp_file("resolver_unsupported_cert");
        let key = create_temp_file("resolver_unsupported_key");
        let conf = build_server_conf(&cert, &key, "dns tcp https://dns.example.com/dns-query;");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("unsupported dns resolver kind should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("unsupported resolver kind `tcp`")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_resolver_config_rejects_invalid_uri() {
        let cert = create_temp_file("resolver_invalid_uri_cert");
        let key = create_temp_file("resolver_invalid_uri_key");
        let conf = build_server_conf(&cert, &key, "dns h3 http://[::1;");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("resolver URI should parse fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid dns resolver uri directive value")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_dns_resolver_and_publisher() {
        let cert = create_temp_file("dns_cert");
        let key = create_temp_file("dns_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; dns h3 https://dns.example.com/dns-query; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let document = parse_doc(&conf);
        let server = first_server(&document);
        let resolver = server
            .require::<ResolverConfig>("dns")
            .expect("dns should exist");

        assert_eq!(resolver.0.to_string(), "https://dns.example.com/dns-query");

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_server_names_with_multiple_values() {
        let cert = create_temp_file("server_names_cert");
        let key = create_temp_file("server_names_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name main.example.com backup.example.com; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let document = parse_doc(&conf);
        let server = first_server(&document);
        let names = server
            .require::<ServerNames>("server_name")
            .expect("server_name should be typed")
            .0
            .clone();

        assert_eq!(names.len(), 2);
        assert_eq!(names[0].name.as_partial(), "main.example.com");
        assert_eq!(names[1].name.as_partial(), "backup.example.com");

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_server_names_rejects_invalid_value() {
        let cert = create_temp_file("server_names_invalid_cert");
        let key = create_temp_file("server_names_invalid_key");
        let conf = build_server_conf(&cert, &key, "server_name invalid_host@@; ");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("invalid server_name should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid server_name directive value")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_listen_config_with_interface_only() {
        let cert = create_temp_file("listen_interface_only_cert");
        let key = create_temp_file("listen_interface_only_key");
        let conf = format!(
            "pishoo {{ server {{ listen all; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let server = first_server(&parse_doc(&conf));
        let listen = server
            .require::<ListenConfig>("listen")
            .expect("listen should be typed")
            .0[0]
            .clone();
        assert_eq!(listen.port, 0);
        assert_eq!(listen.range, crate::parse::types::IfaceRange::All);

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_listen_config_with_address_and_families() {
        let cert = create_temp_file("listen_address_cert");
        let key = create_temp_file("listen_address_key");
        let conf = format!(
            "pishoo {{ server {{ listen 127.0.0.1:443,127.0.0.2:8443; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let server = first_server(&parse_doc(&conf));
        let listen = server
            .require::<ListenConfig>("listen")
            .expect("listen should be typed");

        assert_eq!(listen.0.len(), 1);
        let only = &listen.0[0];
        assert_eq!(only.port, 0);
        assert!(only.specific_addrs.is_some());

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_listen_config_with_interface_and_port() {
        let cert = create_temp_file("listen_iface_port_cert");
        let key = create_temp_file("listen_iface_port_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 8443; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let server = first_server(&parse_doc(&conf));
        let listen = server
            .require::<ListenConfig>("listen")
            .expect("listen should be typed")
            .0[0]
            .clone();
        assert_eq!(listen.port, 8443);

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_listen_config_rejects_invalid_family() {
        let cert = create_temp_file("listen_bad_family_cert");
        let key = create_temp_file("listen_bad_family_key");
        let conf = build_server_conf(&cert, &key, "listen all ipv9 8443;");

        let failure =
            crate::parse::parse_config_str_for_test(&conf).expect_err("invalid family should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid listen ip family")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_listen_config_with_interface_and_family() {
        let cert = create_temp_file("listen_iface_family_cert");
        let key = create_temp_file("listen_iface_family_key");
        let conf = format!(
            "pishoo {{ server {{ listen eth0 v4only; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let server = first_server(&parse_doc(&conf));
        let listen = server
            .require::<ListenConfig>("listen")
            .expect("listen should be typed")
            .0[0]
            .clone();

        assert_eq!(listen.port, 0);
        assert_eq!(listen.families, crate::parse::types::IpFamilies::V4);
        assert_eq!(
            listen.range,
            crate::parse::types::IfaceRange::Exact("eth0".to_owned())
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_listen_config_with_interface_family_and_port() {
        let cert = create_temp_file("listen_iface_family_port_cert");
        let key = create_temp_file("listen_iface_family_port_key");
        let conf = format!(
            "pishoo {{ server {{ listen eth0 v6only 9443; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let server = first_server(&parse_doc(&conf));
        let listen = server
            .require::<ListenConfig>("listen")
            .expect("listen should be typed")
            .0[0]
            .clone();

        assert_eq!(listen.port, 9443);
        assert_eq!(listen.families, crate::parse::types::IpFamilies::V6);
        assert_eq!(
            listen.range,
            crate::parse::types::IfaceRange::Exact("eth0".to_owned())
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_listen_config_rejects_too_many_arguments() {
        let cert = create_temp_file("listen_too_many_args_cert");
        let key = create_temp_file("listen_too_many_args_key");
        let conf = build_server_conf(&cert, &key, "listen all v4only 443 extra;");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("too many listen args should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid listen directive argument count")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_gzip_comp_level_rejects_invalid_value() {
        let conf = "pishoo { server { listen all 5378; server_name example.com; ssl_certificate /tmp/server.pem; ssl_certificate_key /tmp/key.pem; gzip_comp_level bad; } }";

        let failure = crate::parse::parse_config_str_for_test(conf)
            .expect_err("invalid gzip_comp_level should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("failed to parse directive `gzip_comp_level`")
        );
    }

    #[test]
    fn parse_mime_types_directive_parses_block() {
        let cert = create_temp_file("mime_types_cert");
        let key = create_temp_file("mime_types_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "types { text/plain txt; application/json json; }",
        );

        let server = first_server(&parse_doc(&conf));
        let types = server
            .require::<MimeTypes>("types")
            .expect("types should be typed")
            .0
            .clone();

        assert_eq!(types.get("txt").unwrap().to_str().unwrap(), "text/plain");
        assert_eq!(
            types.get("json").unwrap().to_str().unwrap(),
            "application/json"
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_mime_types_rejects_non_block_form() {
        let cert = create_temp_file("mime_types_invalid_cert");
        let key = create_temp_file("mime_types_invalid_key");
        let conf = build_server_conf(&cert, &key, "types text/plain txt;");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("mime type map requires block form");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("types directive must be a block")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_gzip_numeric_directives_reject_invalid_values() {
        let cert = create_temp_file("gzip_numeric_cert");
        let key = create_temp_file("gzip_numeric_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; gzip_min_length invalid; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("invalid gzip number should fail");
        let report = snafu::Report::from_error(&failure.error).to_string();

        assert!(report.contains("failed to parse directive `gzip_min_length`"));
        assert_error_chain_display_single_line(&failure.error);

        cleanup_temp_files(&[&cert, &key]);
    }
}

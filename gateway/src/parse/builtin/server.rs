use std::path::PathBuf;

use dhttp_home::identity::ssl::{CERT_FILE_NAME, KEY_FILE_NAME};
use snafu::{OptionExt, Snafu, ensure};

use crate::parse::{
    document::ConfigNode,
    registry::{BuildOptions, ConfigRegistry, ContextKey, DirectiveSpec, MergePolicy, context},
    source::SourceSpan,
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, ListenConfig,
        MimeTypes, PathConfig, ResolverConfig, ServerIdConfig, ServerName, ServerNames, StringList,
    },
    value::TypedValue,
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum FinalizeServerError {
    #[snafu(display("missing listen directive in server context"))]
    MissingListen { span: SourceSpan },
    #[snafu(display("missing ssl_certificate directive in server context"))]
    MissingCertificate { span: SourceSpan },
    #[snafu(display("missing ssl_certificate_key directive in server context"))]
    MissingCertificateKey { span: SourceSpan },
    #[snafu(display("default ssl_certificate path does not exist"))]
    MissingDefaultCertificate { span: SourceSpan, path: PathBuf },
    #[snafu(display("default ssl_certificate_key path does not exist"))]
    MissingDefaultCertificateKey { span: SourceSpan, path: PathBuf },
}

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::SERVER,
        finalize: Some(finalize_server),
    });
    registry.register_directive(context::ROOT, server_block(context::ROOT));
    registry.register_directive(context::PISHOO, server_block(context::PISHOO));
    register_server_leaf::<ListenConfig>(registry, "listen", MergePolicy::Append);
    register_server_leaf::<ServerNames>(registry, "server_name", MergePolicy::RejectDuplicate);
    register_server_leaf::<ServerIdConfig>(registry, "server_id", MergePolicy::RejectDuplicate);
    register_server_leaf::<ResolverConfig>(registry, "dns", MergePolicy::RejectDuplicate);
    register_server_leaf::<BoolConfig>(registry, "gzip", MergePolicy::RejectDuplicate);
    register_server_leaf::<BoolConfig>(registry, "gzip_vary", MergePolicy::RejectDuplicate);
    register_server_leaf::<GzipMinLength>(
        registry,
        "gzip_min_length",
        MergePolicy::RejectDuplicate,
    );
    register_server_leaf::<GzipCompLevel>(
        registry,
        "gzip_comp_level",
        MergePolicy::RejectDuplicate,
    );
    register_server_leaf::<StringList>(registry, "gzip_types", MergePolicy::RejectDuplicate);
    register_server_leaf::<PathConfig>(registry, "ssl_certificate", MergePolicy::RejectDuplicate);
    register_server_leaf::<PathConfig>(
        registry,
        "ssl_certificate_key",
        MergePolicy::RejectDuplicate,
    );
    register_server_leaf::<DefaultType>(registry, "default_type", MergePolicy::RejectDuplicate);
    register_server_leaf::<AccessRulesUri>(registry, "access_rules", MergePolicy::RejectDuplicate);
    register_server_leaf::<BoolConfig>(registry, "relay", MergePolicy::RejectDuplicate);
    register_server_leaf::<BoolConfig>(registry, "stun", MergePolicy::RejectDuplicate);
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::raw_value::<MimeTypes>(
            "types",
            vec![context::SERVER],
            MergePolicy::RejectDuplicate,
        ),
    );
}

fn server_block(parent: ContextKey) -> DirectiveSpec {
    DirectiveSpec::context_empty("server", vec![parent], context::SERVER, MergePolicy::Append)
}

fn register_server_leaf<T>(registry: &mut ConfigRegistry, name: &'static str, merge: MergePolicy)
where
    T: crate::parse::registry::DirectiveValue,
    for<'input, 'directive> T: TryFrom<
            &'input crate::parse::registry::DirectiveInput<'directive>,
            Error = <T as crate::parse::registry::DirectiveValue>::Error,
        >,
{
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::leaf_value::<T>(name, vec![context::SERVER], merge),
    );
}

fn finalize_server(
    node: &mut ConfigNode,
    options: &BuildOptions<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ensure!(
        !node.get_all::<ListenConfig>("listen")?.is_empty(),
        finalize_server_error::MissingListenSnafu { span: node.span }
    );
    if let Some(identity_profile) = options.identity_profile {
        if node.get::<ServerNames>("server_name")?.is_none() {
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
        if node.get::<PathConfig>("ssl_certificate")?.is_none() {
            let path = identity_profile.ssl_dir().join(CERT_FILE_NAME);
            ensure!(
                path.exists(),
                finalize_server_error::MissingDefaultCertificateSnafu {
                    span: node.span,
                    path
                }
            );
            node.insert_slot(
                "ssl_certificate",
                TypedValue::new(PathConfig(path), node.span),
            );
        }
        if node.get::<PathConfig>("ssl_certificate_key")?.is_none() {
            let path = identity_profile.ssl_dir().join(KEY_FILE_NAME);
            ensure!(
                path.exists(),
                finalize_server_error::MissingDefaultCertificateKeySnafu {
                    span: node.span,
                    path
                }
            );
            node.insert_slot(
                "ssl_certificate_key",
                TypedValue::new(PathConfig(path), node.span),
            );
        }
    }
    node.get::<PathConfig>("ssl_certificate")?
        .context(finalize_server_error::MissingCertificateSnafu { span: node.span })?;
    node.get::<PathConfig>("ssl_certificate_key")?
        .context(finalize_server_error::MissingCertificateKeySnafu { span: node.span })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::parse::{
        tests::{
            assert_error_chain_display_single_line, build_server_conf, cleanup_temp_files,
            create_temp_file, first_pishoo, first_server, parse_doc,
        },
        types::{
            AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, ListenConfig, MimeTypes,
            PathConfig, ResolverConfig, ServerIdConfig, ServerNames,
        },
    };

    #[test]
    fn parse_server_with_server_id() {
        let cert = create_temp_file("server_id_cert");
        let key = create_temp_file("server_id_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; server_id 1; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let document = parse_doc(&conf);
        let server = first_server(&document);

        let names = server
            .require::<ServerNames>("server_name")
            .expect("server_name should exist");
        assert_eq!(names.0[0].name.as_partial(), "example.com");
        let id = server
            .require::<ServerIdConfig>("server_id")
            .expect("server_id should exist");
        assert_eq!(id.0, 1);

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_multiple_servers_with_different_ids() {
        let cert1 = create_temp_file("server1_cert");
        let key1 = create_temp_file("server1_key");
        let cert2 = create_temp_file("server2_cert");
        let key2 = create_temp_file("server2_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name main.example.com; server_id 0; ssl_certificate {}; ssl_certificate_key {}; }} server {{ listen all 5379; server_name backup.example.com; server_id 1; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert1.display(),
            key1.display(),
            cert2.display(),
            key2.display()
        );

        let document = parse_doc(&conf);
        let pishoo = first_pishoo(&document);
        let servers = pishoo.children("server").expect("servers should exist");

        assert_eq!(servers.len(), 2);
        assert_eq!(
            servers[0].require::<ServerIdConfig>("server_id").unwrap().0,
            0
        );
        assert_eq!(
            servers[1].require::<ServerIdConfig>("server_id").unwrap().0,
            1
        );

        cleanup_temp_files(&[&cert1, &key1, &cert2, &key2]);
    }

    #[test]
    fn parse_server_without_server_id() {
        let cert = create_temp_file("no_server_id_cert");
        let key = create_temp_file("no_server_id_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            cert.display(),
            key.display()
        );

        let document = parse_doc(&conf);
        let server = first_server(&document);

        assert!(
            server
                .get::<ServerIdConfig>("server_id")
                .expect("typed query should succeed")
                .is_none()
        );

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
                .require::<PathConfig>("ssl_certificate")
                .expect("ssl_certificate should be typed")
                .0,
            cert.clone()
        );
        assert_eq!(
            server
                .require::<PathConfig>("ssl_certificate_key")
                .expect("ssl_certificate_key should be typed")
                .0,
            key.clone()
        );

        cleanup_temp_files(&[&cert, &key]);
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

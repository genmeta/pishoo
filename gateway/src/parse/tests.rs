use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::parse::{
    document::{ConfigDocument, ConfigNode},
    types::{
        AccessRulesUri, ClientNameConfig, GzipCompLevel, GzipMinLength, PathConfig, ProxyPass,
        ResolverConfig, ServerIdConfig, ServerNames, SocketAddrs, SshLoginMethods,
    },
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn create_temp_file(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "gateway_{prefix}_{}_{}.pem",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, "dummy").expect("write temp config fixture");
    path
}

fn cleanup_temp_files(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

fn parse_doc(conf: &str) -> ConfigDocument {
    crate::parse::parse_config_str_for_test(conf).expect("config should parse")
}

fn first_pishoo(document: &ConfigDocument) -> Arc<ConfigNode> {
    document.root.children("pishoo").expect("pishoo children")[0].clone()
}

fn first_server(document: &ConfigDocument) -> Arc<ConfigNode> {
    first_pishoo(document)
        .children("server")
        .expect("server children")[0]
        .clone()
}

fn build_proxy_conf(server_cert: &Path, server_key: &Path, location_body: &str) -> String {
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

#[test]
fn parse_stun_change_port_rejects_invalid_value() {
    let cert = create_temp_file("stun_port_cert");
    let key = create_temp_file("stun_port_key");
    let conf = format!(
        "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; stun_server {{ bind 127.0.0.1:20000; change_port invalid; }} }} }}",
        cert.display(),
        key.display()
    );

    let failure = crate::parse::parse_config_str_for_test(&conf)
        .expect_err("invalid change_port should fail");
    let report = snafu::Report::from_error(&failure.error).to_string();

    assert!(report.contains("failed to parse directive `change_port`"));
    assert_error_chain_display_single_line(&failure.error);

    cleanup_temp_files(&[&cert, &key]);
}

#[test]
fn parse_numeric_and_identity_directives_keep_typed_values() {
    let conf = "pishoo { gzip_min_length 1100; gzip_comp_level 6; proxy { listen 127.0.0.1:8080; client_name example.com; } }";

    let document = parse_doc(conf);
    let pishoo = first_pishoo(&document);
    let proxy = pishoo.children("proxy").expect("proxy should exist")[0].clone();

    assert_eq!(
        pishoo
            .require::<GzipMinLength>("gzip_min_length")
            .expect("gzip_min_length should be typed")
            .0,
        1100
    );
    assert_eq!(
        pishoo
            .require::<GzipCompLevel>("gzip_comp_level")
            .expect("gzip_comp_level should be typed")
            .0,
        6
    );
    assert_eq!(
        proxy
            .require::<ClientNameConfig>("client_name")
            .expect("client_name should be typed")
            .0
            .as_partial(),
        "example.com"
    );
}

#[test]
fn parse_pid_and_access_rules_keep_domain_types() {
    let conf = "pishoo { pid /tmp/pishoo-test.pid; access_rules sqlite:///tmp/rules.db?mode=ro; }";

    let document = parse_doc(conf);
    let pishoo = first_pishoo(&document);

    assert_eq!(
        pishoo
            .require::<PathConfig>("pid")
            .expect("pid should be typed")
            .0,
        PathBuf::from("/tmp/pishoo-test.pid")
    );
    assert_eq!(
        pishoo
            .require::<AccessRulesUri>("access_rules")
            .expect("access_rules should be typed")
            .0
            .as_str(),
        "sqlite:///tmp/rules.db?mode=ro"
    );
}

#[test]
fn parse_access_rules_rejects_invalid_uri() {
    let conf = "pishoo { access_rules not-a-uri; }";

    let failure = crate::parse::parse_config_str_for_test(conf)
        .expect_err("invalid access_rules URI should fail");
    let report = snafu::Report::from_error(&failure.error).to_string();

    assert!(report.contains("failed to parse directive `access_rules`"));
    assert_error_chain_display_single_line(&failure.error);
}

#[test]
fn parse_address_directives_keep_socket_addrs_type() {
    let conf = "pishoo { proxy { listen 127.0.0.1:8080; } }";

    let document = parse_doc(conf);
    let pishoo = first_pishoo(&document);
    let proxy = pishoo.children("proxy").expect("proxy should exist")[0].clone();

    assert_eq!(
        proxy
            .require::<SocketAddrs>("listen")
            .expect("proxy listen should be typed")
            .0,
        vec!["127.0.0.1:8080".parse().expect("address should parse")]
    );
}

#[test]
fn parse_ssh_login_keeps_semantic_type() {
    let cert = create_temp_file("ssh_login_cert");
    let key = create_temp_file("ssh_login_key");
    let conf = build_proxy_conf(&cert, &key, "ssh_login ssl;");

    let document = parse_doc(&conf);
    let location = first_server(&document)
        .children("location")
        .expect("location exists")[0]
        .clone();

    assert_eq!(
        location
            .require::<SshLoginMethods>("ssh_login")
            .expect("ssh_login should be typed")
            .0,
        vec!["ssl".to_owned()]
    );

    cleanup_temp_files(&[&cert, &key]);
}

#[test]
fn parse_proxy_pass_rejects_unsupported_scheme() {
    let cert = create_temp_file("proxy_scheme_cert");
    let key = create_temp_file("proxy_scheme_key");
    let conf = build_proxy_conf(&cert, &key, "proxy_pass ftp://backend.example.com;");

    let failure =
        crate::parse::parse_config_str_for_test(&conf).expect_err("unsupported scheme should fail");
    let report = snafu::Report::from_error(&failure.error).to_string();

    assert!(report.contains("unsupported proxy_pass uri scheme"));
    assert_error_chain_display_single_line(&failure.error);

    cleanup_temp_files(&[&cert, &key]);
}

#[test]
fn parse_https_proxy_with_optional_trusted_certificate() {
    let cert = create_temp_file("https_cert");
    let key = create_temp_file("https_key");
    let trusted = create_temp_file("trusted_ca");
    let conf = build_proxy_conf(
        &cert,
        &key,
        &format!(
            "proxy_pass https://backend.example.com; proxy_ssl_trusted_certificate {};",
            trusted.display()
        ),
    );

    let document = parse_doc(&conf);
    let location = first_server(&document)
        .children("location")
        .expect("location exists")[0]
        .clone();

    assert!(location.require::<ProxyPass>("proxy_pass").is_ok());
    assert!(
        location
            .require::<PathConfig>("proxy_ssl_trusted_certificate")
            .is_ok()
    );

    cleanup_temp_files(&[&cert, &key, &trusted]);
}

#[test]
fn parse_proxy_ssl_certificate_requires_matching_key() {
    let cert = create_temp_file("pair_cert");
    let key = create_temp_file("pair_key");
    let client_cert = create_temp_file("client_cert");
    let conf = build_proxy_conf(
        &cert,
        &key,
        &format!(
            "proxy_pass https://backend.example.com; proxy_ssl_certificate {};",
            client_cert.display()
        ),
    );

    let failure =
        crate::parse::parse_config_str_for_test(&conf).expect_err("missing proxy key should fail");

    assert!(
        snafu::Report::from_error(&failure.error)
            .to_string()
            .contains("failed to finalize configuration context")
    );

    cleanup_temp_files(&[&cert, &key, &client_cert]);
}

#[test]
fn parse_http_proxy_allows_proxy_ssl_directives() {
    let cert = create_temp_file("http_cert");
    let key = create_temp_file("http_key");
    let client_cert = create_temp_file("http_client_cert");
    let client_key = create_temp_file("http_client_key");
    let trusted = create_temp_file("http_trusted");
    let conf = build_proxy_conf(
        &cert,
        &key,
        &format!(
            "proxy_pass http://backend.example.com; proxy_ssl_certificate {}; proxy_ssl_certificate_key {}; proxy_ssl_trusted_certificate {};",
            client_cert.display(),
            client_key.display(),
            trusted.display()
        ),
    );

    let document = parse_doc(&conf);
    let location = first_server(&document)
        .children("location")
        .expect("location exists")[0]
        .clone();

    assert!(
        location
            .require::<PathConfig>("proxy_ssl_certificate")
            .is_ok()
    );
    assert!(
        location
            .require::<PathConfig>("proxy_ssl_certificate_key")
            .is_ok()
    );

    cleanup_temp_files(&[&cert, &key, &client_cert, &client_key, &trusted]);
}

#[test]
fn diagnostic_contains_source_snippet_but_display_is_single_line() {
    let conf = "pishoo { server { listen all 5378; server_name example.com; ssl_certificate /missing/cert.pem; ssl_certificate_key /missing/key.pem; location /api { proxy_pass ftp://backend.example.com; } } }";
    let failure = crate::parse::parse_config_str_for_test(conf).expect_err("config should fail");
    let report = snafu::Report::from_error(&failure.error).to_string();
    let diagnostic = failure.diagnostic().to_string();

    assert!(report.contains("unsupported proxy_pass uri scheme"));
    assert_error_chain_display_single_line(&failure.error);
    assert!(diagnostic.contains("proxy_pass ftp://backend.example.com"));
    assert!(diagnostic.contains("^"));
}

fn assert_error_chain_display_single_line(error: &(dyn std::error::Error + 'static)) {
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

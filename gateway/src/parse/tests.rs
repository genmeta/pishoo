use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::Arc,
};

use super::{
    ParsedPishooConfig, TypedConfigParser,
    config::{
        AccessLogDirective, LocationConfig, OriginScope, ResolvedAccessLogConfig, ServerConfig,
    },
    error::ConfigLoadFailure,
};

pub(crate) fn parse_root(text: &str) -> Result<ParsedPishooConfig, ConfigLoadFailure> {
    TypedConfigParser::new().parse_root(text, Path::new("/tmp/pishoo.conf"), None)
}

pub(crate) fn parse_direct_server(extra: &str) -> Result<ServerConfig, String> {
    let text = format!(
        "pishoo {{ server {{ listen all 5378; server_name example.com; \
         ssl_certificate /tmp/server.crt; ssl_certificate_key /tmp/server.key; {extra} }} }}"
    );
    let parsed =
        parse_root(&text).map_err(|error| snafu::Report::from_error(&error).to_string())?;
    match parsed
        .servers()
        .first()
        .expect("fixture has one server")
        .result()
    {
        Ok(server) => Ok(server.clone()),
        Err(error) => Err(snafu::Report::from_error(error).to_string()),
    }
}

pub(crate) fn first_server(text: &str) -> Result<ServerConfig, String> {
    let parsed = parse_root(text).map_err(|error| snafu::Report::from_error(&error).to_string())?;
    parsed
        .into_parts()
        .1
        .into_vec()
        .into_iter()
        .next()
        .expect("fixture has one server")
        .into_result()
        .map_err(|error| snafu::Report::from_error(&error).to_string())
}

pub(crate) fn first_location(text: &str) -> Result<LocationConfig, String> {
    first_server(text)?
        .locations()
        .first()
        .cloned()
        .ok_or_else(|| "fixture has no location".to_owned())
}

pub(crate) fn parse_location(body: &str) -> Result<Arc<LocationConfig>, String> {
    parse_location_pattern("/", body)
}

pub(crate) fn parse_location_pattern(
    pattern: &str,
    body: &str,
) -> Result<Arc<LocationConfig>, String> {
    let server = parse_direct_server(&format!("location {pattern} {{ {body} }}"))?;
    Ok(Arc::new(
        server
            .locations()
            .first()
            .expect("fixture has one location")
            .clone(),
    ))
}

pub(crate) fn build_server_conf(cert: &Path, key: &Path, body: &str) -> String {
    format!(
        "pishoo {{ server {{ listen all 5378; server_name example.com; \
         ssl_certificate {}; ssl_certificate_key {}; {body} }} }}",
        cert.display(),
        key.display(),
    )
}

pub(crate) fn build_proxy_conf(cert: &Path, key: &Path, body: &str) -> String {
    build_server_conf(cert, key, &format!("location / {{ {body} }}"))
}

pub(crate) fn create_temp_file(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "gateway-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos(),
    ));
    std::fs::write(&path, b"fixture").expect("create temporary fixture");
    path
}

pub(crate) fn cleanup_temp_files(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn assert_error_chain_display_single_line(error: &(dyn Error + 'static)) {
    let mut current = Some(error);
    while let Some(error) = current {
        assert!(!error.to_string().contains('\n'));
        current = error.source();
    }
}

#[test]
fn concrete_tree_computes_full_lineage_during_construction() {
    let root = TypedConfigParser::new()
        .parse_root("pishoo { gzip off; }", Path::new("/tmp/root.conf"), None)
        .unwrap();
    let root_defaults = root.pishoo().worker_defaults();
    let home = dhttp::home::DhttpHome::for_user_home_dir(PathBuf::from("/tmp/home"));
    let worker = TypedConfigParser::new()
        .parse_worker(
            "pishoo { gzip on; }",
            Path::new("/tmp/home/.config/dhttp/pishoo.conf"),
            &home,
            &root_defaults,
        )
        .unwrap();
    let worker_defaults = worker.pishoo().worker_defaults();
    let profile = home.identity_profile("example.com".parse().unwrap());
    let identity = TypedConfigParser::new()
        .parse_identity(
            "server { listen all 443; location / { gzip off; } }",
            Path::new("/tmp/home/.config/dhttp/identity/example.com/server.conf"),
            profile,
            &worker_defaults,
        )
        .unwrap();
    let server = identity.result().as_ref().unwrap();
    let gzip = server.locations()[0].http().gzip();

    assert!(!gzip.effective().0);
    assert_eq!(
        gzip.lineage()
            .iter()
            .map(|origin| origin.scope())
            .collect::<Vec<_>>(),
        [
            OriginScope::Builtin,
            OriginScope::RootPishoo,
            OriginScope::WorkerPishoo,
            OriginScope::Location,
        ]
    );
}

#[test]
fn lineage_points_to_the_explicit_directive() {
    let parsed = TypedConfigParser::new()
        .parse_root(
            "pishoo {\n    gzip on;\n}",
            Path::new("/etc/pishoo/pishoo.conf"),
            None,
        )
        .unwrap();
    let origin = parsed.pishoo().http().gzip().lineage().last().unwrap();

    assert_eq!(origin.path(), Some(Path::new("/etc/pishoo/pishoo.conf")));
    assert_eq!(origin.line(), Some(2));
    assert_eq!(origin.column(), Some(5));
}

#[test]
fn direct_server_semantic_failure_is_isolated_as_candidate() {
    let parsed = parse_root(
        "pishoo { server { listen all 443; server_name bad.example; } \
         server { listen all 444; server_name good.example; ssl_certificate /tmp/c; ssl_certificate_key /tmp/k; } }",
    )
    .unwrap();

    assert!(parsed.servers()[0].result().is_err());
    assert!(parsed.servers()[1].result().is_ok());
}

#[test]
fn duplicate_scalar_directive_is_rejected() {
    let failure = parse_root("pishoo { gzip on; gzip off; }").unwrap_err();
    assert!(
        snafu::Report::from_error(&failure)
            .to_string()
            .contains("duplicate directive `gzip`")
    );
}

#[test]
fn server_accepts_multiple_location_blocks() {
    let server = parse_direct_server("location / {} location /files {}").unwrap();

    assert_eq!(server.locations().len(), 2);
}

#[test]
fn root_snapshot_checked_serde_round_trip_preserves_every_field() {
    let parsed = parse_root(
        "pishoo { gzip on; gzip_vary on; gzip_min_length 91; gzip_comp_level 4; \
         gzip_types text/plain application/json; default_type application/octet-stream; \
         access_log /tmp/access.log; }",
    )
    .unwrap();
    let expected = parsed.pishoo().worker_defaults();
    let bytes = serde_json::to_vec(&expected).unwrap();
    let actual = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(expected, actual);
}

#[test]
fn access_log_defaults_follow_server_identity_ownership() {
    let direct = parse_direct_server("").unwrap();
    assert_eq!(
        direct.http().access_log().effective(),
        &AccessLogDirective::Off
    );

    let home = dhttp::home::DhttpHome::for_user_home_dir(PathBuf::from("/tmp/alice"));
    let profile = home.identity_profile("alice.dhttp.net".parse().unwrap());
    let defaults = TypedConfigParser::new()
        .parse_root(
            "pishoo {}",
            Path::new("/etc/pishoo/pishoo.conf"),
            Some(&home),
        )
        .unwrap()
        .pishoo()
        .worker_defaults();
    let identity = TypedConfigParser::new()
        .parse_identity(
            "server { listen all 443; location / {} }",
            &profile.server_conf_path(),
            profile.clone(),
            &defaults,
        )
        .unwrap();
    let server = identity.result().as_ref().unwrap();

    assert_eq!(
        server.http().access_log().effective(),
        &AccessLogDirective::ProfileDefault
    );
    assert_eq!(
        server
            .http()
            .access_log()
            .effective()
            .materialize(server.identity())
            .unwrap(),
        ResolvedAccessLogConfig::Enabled(
            super::domain::ResolvedConfigPath::try_from(profile.access_log_path()).unwrap()
        )
    );
}

#[test]
fn location_access_log_override_keeps_location_lineage() {
    let server = first_server(
        "pishoo { access_log /tmp/root.log; server { listen all 443; \
         server_name example.com; ssl_certificate /tmp/c; ssl_certificate_key /tmp/k; \
         location /private { access_log off; } } }",
    )
    .unwrap();
    let access_log = server.locations()[0].http().access_log();

    assert_eq!(access_log.effective(), &AccessLogDirective::Off);
    assert_eq!(
        access_log.lineage().last().unwrap().scope(),
        OriginScope::Location
    );
}

#[test]
fn root_access_log_path_survives_worker_snapshot_without_rebasing() {
    let home = dhttp::home::DhttpHome::for_user_home_dir(PathBuf::from("/home/alice"));
    let root = TypedConfigParser::new()
        .parse_root(
            "pishoo { access_log logs/access.log; }",
            Path::new("/etc/pishoo/pishoo.conf"),
            Some(&home),
        )
        .unwrap()
        .pishoo()
        .worker_defaults();
    let worker = TypedConfigParser::new()
        .parse_worker(
            "pishoo {}",
            Path::new("/home/alice/.config/dhttp/pishoo.conf"),
            &home,
            &root,
        )
        .unwrap();

    assert_eq!(
        worker
            .pishoo()
            .http()
            .access_log()
            .effective()
            .resolved_path(),
        Some(Path::new("/etc/pishoo/logs/access.log"))
    );
}

use http::{HeaderName, HeaderValue};
use snafu::{OptionExt, Snafu, ensure};

use crate::parse::{
    document::ConfigNode,
    domain::ResolvedConfigPath,
    pattern::{ParsePatternError, Pattern},
    registry::{
        BuildOptions, CascadePolicy, ConfigRegistry, ContextPayloadKey, DirectiveInput,
        DirectiveValue, DuplicatePolicy, LocalDirectiveKey, PayloadCardinality, ReloadImpact,
        RepeatedCardinality, RepeatedDirectiveKey, SingleCardinality, TransportPolicy,
        TypedDirectiveDefinition, context,
    },
    source::SourceSpan,
    types::{
        BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, HeaderRule, HeaderRules, MimeTypes,
        ProxyPass, SshLoginMethods, SshSslUsers, StringList,
    },
    value::TypedValue,
};

macro_rules! single_definition {
    ($definition:ident, $key:ident, $value:ty, $name:literal) => {
        const $definition: TypedDirectiveDefinition<$value, SingleCardinality> =
            TypedDirectiveDefinition::single_leaf(
                context::LOCATION,
                $name,
                DuplicatePolicy::Reject,
                CascadePolicy::NearestWins,
                TransportPolicy::WorkerLocalOnly,
                ReloadImpact::RuntimeState,
            );
        pub(crate) const $key: LocalDirectiveKey<$value> = $definition.key();
    };
}

macro_rules! repeated_definition {
    ($definition:ident, $key:ident, $value:ty, $name:literal) => {
        const $definition: TypedDirectiveDefinition<$value, RepeatedCardinality> =
            TypedDirectiveDefinition::repeated_leaf(
                context::LOCATION,
                $name,
                CascadePolicy::NearestWins,
                TransportPolicy::WorkerLocalOnly,
                ReloadImpact::RuntimeState,
            );
        pub(crate) const $key: RepeatedDirectiveKey<$value> = $definition.key();
    };
}

const PATTERN_DEFINITION: TypedDirectiveDefinition<Pattern, PayloadCardinality> =
    TypedDirectiveDefinition::payload(
        context::SERVER,
        context::LOCATION,
        "location",
        DuplicatePolicy::Append,
        CascadePolicy::None,
        TransportPolicy::WorkerLocalOnly,
        ReloadImpact::RuntimeState,
    );
pub(crate) const PATTERN_KEY: ContextPayloadKey<Pattern> = PATTERN_DEFINITION.key();
single_definition!(ROOT_DEFINITION, ROOT_KEY, ResolvedConfigPath, "root");
single_definition!(ALIAS_DEFINITION, ALIAS_KEY, ResolvedConfigPath, "alias");
single_definition!(GZIP_DEFINITION, GZIP_KEY, BoolConfig, "gzip");
single_definition!(GZIP_VARY_DEFINITION, GZIP_VARY_KEY, BoolConfig, "gzip_vary");
single_definition!(
    GZIP_MIN_LENGTH_DEFINITION,
    GZIP_MIN_LENGTH_KEY,
    GzipMinLength,
    "gzip_min_length"
);
single_definition!(
    GZIP_COMP_LEVEL_DEFINITION,
    GZIP_COMP_LEVEL_KEY,
    GzipCompLevel,
    "gzip_comp_level"
);
single_definition!(
    GZIP_TYPES_DEFINITION,
    GZIP_TYPES_KEY,
    StringList,
    "gzip_types"
);
single_definition!(INDEX_DEFINITION, INDEX_KEY, StringList, "index");
repeated_definition!(
    ADD_HEADER_DEFINITION,
    ADD_HEADER_KEY,
    HeaderRules,
    "add_header"
);
repeated_definition!(
    PROXY_SET_HEADER_DEFINITION,
    PROXY_SET_HEADER_KEY,
    HeaderRules,
    "proxy_set_header"
);
single_definition!(
    PROXY_PASS_DEFINITION,
    PROXY_PASS_KEY,
    ProxyPass,
    "proxy_pass"
);
single_definition!(
    PROXY_SSL_CERTIFICATE_DEFINITION,
    PROXY_SSL_CERTIFICATE_KEY,
    ResolvedConfigPath,
    "proxy_ssl_certificate"
);
single_definition!(
    PROXY_SSL_CERTIFICATE_KEY_DEFINITION,
    PROXY_SSL_CERTIFICATE_KEY_KEY,
    ResolvedConfigPath,
    "proxy_ssl_certificate_key"
);
single_definition!(
    PROXY_SSL_TRUSTED_CERTIFICATE_DEFINITION,
    PROXY_SSL_TRUSTED_CERTIFICATE_KEY,
    ResolvedConfigPath,
    "proxy_ssl_trusted_certificate"
);
single_definition!(
    SSH_LOGIN_DEFINITION,
    SSH_LOGIN_KEY,
    SshLoginMethods,
    "ssh_login"
);
repeated_definition!(
    SSH_SSL_USER_DEFINITION,
    SSH_SSL_USER_KEY,
    SshSslUsers,
    "ssh_ssl_user"
);
single_definition!(SSH_DENY_DEFINITION, SSH_DENY_KEY, StringList, "ssh_deny");
single_definition!(
    DEFAULT_TYPE_DEFINITION,
    DEFAULT_TYPE_KEY,
    DefaultType,
    "default_type"
);
const TYPES_DEFINITION: TypedDirectiveDefinition<MimeTypes, SingleCardinality> =
    TypedDirectiveDefinition::raw(
        context::LOCATION,
        "types",
        DuplicatePolicy::Reject,
        CascadePolicy::ReplaceWhole,
        TransportPolicy::WorkerLocalOnly,
        ReloadImpact::RuntimeState,
    );
pub(crate) const TYPES_KEY: LocalDirectiveKey<MimeTypes> = TYPES_DEFINITION.key();

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum FinalizeLocationError {
    #[snafu(display("location context is missing its parsed pattern payload"))]
    MissingPattern { span: SourceSpan },
    #[snafu(display(
        "proxy_ssl_certificate and proxy_ssl_certificate_key must be configured together"
    ))]
    ProxyTlsPair { span: SourceSpan },
    #[snafu(display("proxy_pass cannot include a uri part inside regex location"))]
    ProxyPassUriInRegexLocation { span: SourceSpan },
}

impl DirectiveValue for Pattern {
    type Error = ParsePatternError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for Pattern {
    type Error = ParsePatternError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        crate::parse::pattern::parse_spanned_pattern(&input.directive.args)
    }
}

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::LOCATION,
        finalize: Some(finalize_location),
    });
    PATTERN_DEFINITION.register(registry);
    ROOT_DEFINITION.register(registry);
    ALIAS_DEFINITION.register(registry);
    GZIP_DEFINITION.register(registry);
    GZIP_VARY_DEFINITION.register(registry);
    GZIP_MIN_LENGTH_DEFINITION.register(registry);
    GZIP_COMP_LEVEL_DEFINITION.register(registry);
    GZIP_TYPES_DEFINITION.register(registry);
    INDEX_DEFINITION.register(registry);
    ADD_HEADER_DEFINITION.register(registry);
    PROXY_SET_HEADER_DEFINITION.register(registry);
    PROXY_PASS_DEFINITION.register(registry);
    PROXY_SSL_CERTIFICATE_DEFINITION.register(registry);
    PROXY_SSL_CERTIFICATE_KEY_DEFINITION.register(registry);
    PROXY_SSL_TRUSTED_CERTIFICATE_DEFINITION.register(registry);
    SSH_LOGIN_DEFINITION.register(registry);
    SSH_SSL_USER_DEFINITION.register(registry);
    SSH_DENY_DEFINITION.register(registry);
    DEFAULT_TYPE_DEFINITION.register(registry);
    TYPES_DEFINITION.register(registry);
}

fn finalize_location(
    node: &mut ConfigNode,
    _options: &BuildOptions<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pattern = node
        .payload::<Pattern>()?
        .context(finalize_location_error::MissingPatternSnafu { span: node.span })?;
    let proxy_pass = node.get::<ProxyPass>("proxy_pass")?;
    let has_cert = node
        .get::<ResolvedConfigPath>("proxy_ssl_certificate")?
        .is_some();
    let has_key = node
        .get::<ResolvedConfigPath>("proxy_ssl_certificate_key")?
        .is_some();
    ensure!(
        !matches!(pattern.as_ref(), Pattern::Regex(_) | Pattern::CRegex(_))
            || proxy_pass
                .as_ref()
                .is_none_or(|proxy_pass| !proxy_pass.has_explicit_uri()),
        finalize_location_error::ProxyPassUriInRegexLocationSnafu { span: node.span }
    );
    ensure!(
        has_cert == has_key,
        finalize_location_error::ProxyTlsPairSnafu { span: node.span }
    );
    let server_header = HeaderRules(vec![HeaderRule {
        name: HeaderName::from_static("server"),
        value: HeaderValue::from_static("pishoo"),
        always: true,
    }]);
    node.insert_slot("add_header", TypedValue::new(server_header, node.span));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::parse::{
        domain::ResolvedConfigPath,
        pattern::Pattern,
        tests::{
            build_proxy_conf, build_server_conf, cleanup_temp_files, create_temp_file,
            first_server, parse_doc,
        },
        types::{BoolConfig, DefaultType, HeaderRules, ProxyPass, StringList},
    };

    #[test]
    fn parse_path_list_directives_keep_path_config() {
        let cert = create_temp_file("proxy_path_cert");
        let key = create_temp_file("proxy_path_key");
        let conf = build_proxy_conf(&cert, &key, "root /var/www/site;");

        let server = first_server(&parse_doc(&conf));
        let location = server.children("location").expect("location should exist")[0].clone();

        assert_eq!(
            location
                .require::<ResolvedConfigPath>("root")
                .expect("root should be typed")
                .as_ref()
                .as_ref(),
            PathBuf::from("/var/www/site")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_default_type_in_location_context() {
        let cert = create_temp_file("loc_default_type_cert");
        let key = create_temp_file("loc_default_type_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "location / { root /tmp; default_type text/plain; }",
        );

        let location = first_server(&parse_doc(&conf))
            .children("location")
            .expect("location exists")[0]
            .clone();

        assert_eq!(
            location
                .require::<DefaultType>("default_type")
                .expect("default_type should be typed")
                .0
                .to_str()
                .unwrap(),
            "text/plain"
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_location_alias_index_and_ssh_deny_and_gzip_types() {
        let cert = create_temp_file("loc_alias_index_cert");
        let key = create_temp_file("loc_alias_index_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "location /assets { alias /mnt/assets; index index.html index.txt; ssh_deny ip1 ip2; gzip on; gzip_vary off; gzip_types txt css; }",
        );

        let location = first_server(&parse_doc(&conf))
            .children("location")
            .expect("location exists")[0]
            .clone();

        assert_eq!(
            location
                .require::<ResolvedConfigPath>("alias")
                .expect("alias should be typed")
                .as_ref()
                .as_ref(),
            PathBuf::from("/mnt/assets")
        );
        assert_eq!(
            location
                .require::<StringList>("index")
                .expect("index should be typed")
                .0,
            vec!["index.html", "index.txt"],
        );
        assert_eq!(
            location
                .require::<StringList>("ssh_deny")
                .expect("ssh_deny should be typed")
                .0,
            vec!["ip1", "ip2"]
        );
        assert!(location.require::<BoolConfig>("gzip").expect("gzip").0);
        assert!(
            !location
                .require::<BoolConfig>("gzip_vary")
                .expect("gzip_vary")
                .0
        );
        assert_eq!(
            location
                .require::<StringList>("gzip_types")
                .expect("gzip_types should be typed")
                .0,
            vec!["txt", "css"],
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_location_pattern_variants_from_context_payload() {
        let cert = create_temp_file("pattern_cert");
        let key = create_temp_file("pattern_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "location / { root /tmp; }\nlocation = /login { root /var; }\nlocation ~ \\.(gif|png)$ { root /var/www; }",
        );

        let server = first_server(&parse_doc(&conf));
        let locations = server.children("location").expect("location should exist");

        let common = locations[0]
            .payload::<Pattern>()
            .expect("common payload")
            .expect("common location payload");
        let exact = locations[1]
            .payload::<Pattern>()
            .expect("exact payload")
            .expect("exact location payload");
        let regex = locations[2]
            .payload::<Pattern>()
            .expect("regex payload")
            .expect("regex location payload");

        assert!(matches!(common.as_ref(), Pattern::Common));
        assert!(matches!(exact.as_ref(), Pattern::Exact(_)));
        assert!(matches!(regex.as_ref(), Pattern::Regex(_)));

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_location_pattern_rejects_invalid_argument_count() {
        let cert = create_temp_file("pattern_invalid_cert");
        let key = create_temp_file("pattern_invalid_key");
        let conf = build_server_conf(&cert, &key, "location /foo bar baz { root /tmp; }");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("pattern with invalid arg count should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("number of location args must be 1 or 2")
        );

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

        assert!(
            location
                .require::<crate::parse::types::ProxyPass>("proxy_pass")
                .is_ok()
        );
        assert!(
            location
                .require::<ResolvedConfigPath>("proxy_ssl_trusted_certificate")
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

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("missing proxy key should fail");

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
                .require::<ResolvedConfigPath>("proxy_ssl_certificate")
                .is_ok()
        );
        assert!(
            location
                .require::<ResolvedConfigPath>("proxy_ssl_certificate_key")
                .is_ok()
        );

        cleanup_temp_files(&[&cert, &key, &client_cert, &client_key, &trusted]);
    }

    #[test]
    fn parse_regex_location_rejects_proxy_pass_with_explicit_uri() {
        let cert = create_temp_file("regex_proxy_uri_cert");
        let key = create_temp_file("regex_proxy_uri_key");
        let conf = build_server_conf(
            &cert,
            &key,
            r"location ~ \.php$ { proxy_pass http://backend.example.com/base/; }",
        );

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("regex location proxy_pass with uri should fail");
        let report = snafu::Report::from_error(&failure.error).to_string();
        assert!(report.contains("proxy_pass cannot include a uri part inside regex location"));

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_regex_location_allows_proxy_pass_without_explicit_uri() {
        let cert = create_temp_file("regex_proxy_no_uri_cert");
        let key = create_temp_file("regex_proxy_no_uri_key");
        let conf = build_server_conf(
            &cert,
            &key,
            r"location ~ \.php$ { proxy_pass http://backend.example.com; }",
        );

        let location = first_server(&parse_doc(&conf))
            .children("location")
            .expect("location exists")[0]
            .clone();
        let proxy_pass = location
            .require::<ProxyPass>("proxy_pass")
            .expect("proxy_pass should be typed");
        assert!(!proxy_pass.has_explicit_uri());

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_header_rules_accepts_add_header_and_proxy_set_header() {
        let cert = create_temp_file("header_rules_accept_cert");
        let key = create_temp_file("header_rules_accept_key");
        let conf = build_proxy_conf(
            &cert,
            &key,
            "proxy_set_header X-Trace test; add_header X-Custom yes always;",
        );

        let location = first_server(&parse_doc(&conf))
            .children("location")
            .expect("location exists")[0]
            .clone();

        let proxy_set = location
            .require::<HeaderRules>("proxy_set_header")
            .expect("proxy_set_header should be typed");
        assert_eq!(proxy_set.0.len(), 1);
        assert_eq!(proxy_set.0[0].name.as_str(), "x-trace");
        assert_eq!(
            proxy_set.0[0]
                .value
                .to_str()
                .expect("header value should be valid"),
            "test"
        );
        assert!(proxy_set.0[0].always);

        let add_header_rules = location
            .get_all::<HeaderRules>("add_header")
            .expect("add_header values should be typed");
        let header_names: Vec<_> = add_header_rules
            .iter()
            .flat_map(|entry| entry.0.iter())
            .map(|rule| rule.name.to_string())
            .collect();
        assert!(
            header_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case("x-custom"))
        );
        assert!(
            header_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case("server"))
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_header_rules_rejects_invalid_always_marker() {
        let cert = create_temp_file("header_rules_always_cert");
        let key = create_temp_file("header_rules_always_key");
        let conf = build_proxy_conf(&cert, &key, "add_header X-Trace test sometimes;");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("invalid header add_marker should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid add_header always marker")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[tokio::test]
    async fn parse_location_root_is_relative_to_source_file() {
        let dir = std::env::temp_dir().join(format!(
            "gateway-relative-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("server.crt"), "dummy cert").expect("write cert");
        std::fs::write(dir.join("server.key"), "dummy key").expect("write key");
        std::fs::write(
            dir.join("server.conf"),
            "server {\n    listen all 5378;\n    server_name example.com;\n    ssl_certificate ./server.crt;\n    ssl_certificate_key ./server.key;\n    location / { root .; }\n}\n",
        )
        .expect("write config");

        let registry = crate::parse::default_registry();
        let parsed = crate::parse::load_config_file(
            &dir.join("server.conf"),
            &registry,
            crate::parse::registry::BuildOptions::default(),
        )
        .await
        .expect("config should load");

        let server = parsed.root.children("server").expect("server children")[0].clone();
        let location = server.children("location").expect("location children")[0].clone();
        assert_eq!(
            location
                .require::<crate::parse::domain::ResolvedConfigPath>("root")
                .expect("root should be typed")
                .as_ref()
                .as_ref(),
            dir
        );
    }

    #[tokio::test]
    async fn parse_included_location_root_is_relative_to_included_file() {
        let dir = std::env::temp_dir().join(format!(
            "gateway-relative-include-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(dir.join("sites")).expect("create sites dir");
        std::fs::write(dir.join("server.crt"), "dummy cert").expect("write cert");
        std::fs::write(dir.join("server.key"), "dummy key").expect("write key");
        std::fs::write(
            dir.join("server.conf"),
            "server {\n    listen all 5378;\n    server_name example.com;\n    ssl_certificate ./server.crt;\n    ssl_certificate_key ./server.key;\n    include sites/static.conf;\n}\n",
        )
        .expect("write root config");
        std::fs::write(dir.join("sites/static.conf"), "location / { root .; }\n")
            .expect("write included config");

        let registry = crate::parse::default_registry();
        let parsed = crate::parse::load_config_file(
            &dir.join("server.conf"),
            &registry,
            crate::parse::registry::BuildOptions::default(),
        )
        .await
        .expect("config should load");

        let server = parsed.root.children("server").expect("server children")[0].clone();
        let location = server.children("location").expect("location children")[0].clone();
        assert_eq!(
            location
                .require::<crate::parse::domain::ResolvedConfigPath>("root")
                .expect("root should be typed")
                .as_ref()
                .as_ref(),
            dir.join("sites")
        );
    }
}

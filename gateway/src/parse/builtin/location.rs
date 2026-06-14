use http::{HeaderName, HeaderValue};
use snafu::{Snafu, ensure};

use crate::parse::{
    document::ConfigNode,
    pattern::{ParsePatternError, Pattern},
    registry::{
        BuildOptions, ConfigRegistry, DirectiveInput, DirectiveSpec, DirectiveValue, MergePolicy,
        context,
    },
    source::SourceSpan,
    types::{
        BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, HeaderRule, HeaderRules, MimeTypes,
        PathConfig, ProxyPass, SshLoginMethods, SshSslUsers, StringList,
    },
    value::TypedValue,
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum FinalizeLocationError {
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
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::context_payload::<Pattern>(
            "location",
            vec![context::SERVER],
            context::LOCATION,
            MergePolicy::Append,
        ),
    );
    register_leaf::<PathConfig>(registry, "root", MergePolicy::RejectDuplicate);
    register_leaf::<PathConfig>(registry, "alias", MergePolicy::RejectDuplicate);
    register_leaf::<BoolConfig>(registry, "gzip", MergePolicy::RejectDuplicate);
    register_leaf::<BoolConfig>(registry, "gzip_vary", MergePolicy::RejectDuplicate);
    register_leaf::<GzipMinLength>(registry, "gzip_min_length", MergePolicy::RejectDuplicate);
    register_leaf::<GzipCompLevel>(registry, "gzip_comp_level", MergePolicy::RejectDuplicate);
    register_leaf::<StringList>(registry, "gzip_types", MergePolicy::RejectDuplicate);
    register_leaf::<StringList>(registry, "index", MergePolicy::RejectDuplicate);
    register_leaf::<HeaderRules>(registry, "add_header", MergePolicy::Append);
    register_leaf::<HeaderRules>(registry, "proxy_set_header", MergePolicy::Append);
    register_leaf::<ProxyPass>(registry, "proxy_pass", MergePolicy::RejectDuplicate);
    register_leaf::<PathConfig>(
        registry,
        "proxy_ssl_certificate",
        MergePolicy::RejectDuplicate,
    );
    register_leaf::<PathConfig>(
        registry,
        "proxy_ssl_certificate_key",
        MergePolicy::RejectDuplicate,
    );
    register_leaf::<PathConfig>(
        registry,
        "proxy_ssl_trusted_certificate",
        MergePolicy::RejectDuplicate,
    );
    register_leaf::<SshLoginMethods>(registry, "ssh_login", MergePolicy::RejectDuplicate);
    register_leaf::<SshSslUsers>(registry, "ssh_ssl_user", MergePolicy::Append);
    register_leaf::<StringList>(registry, "ssh_deny", MergePolicy::RejectDuplicate);
    register_leaf::<DefaultType>(registry, "default_type", MergePolicy::RejectDuplicate);
    registry.register_directive(
        context::LOCATION,
        DirectiveSpec::raw_value::<MimeTypes>(
            "types",
            vec![context::LOCATION],
            MergePolicy::RejectDuplicate,
        ),
    );
}

fn register_leaf<T>(registry: &mut ConfigRegistry, name: &'static str, merge: MergePolicy)
where
    T: crate::parse::registry::DirectiveValue,
    for<'input, 'directive> T: TryFrom<
            &'input crate::parse::registry::DirectiveInput<'directive>,
            Error = <T as crate::parse::registry::DirectiveValue>::Error,
        >,
{
    registry.register_directive(
        context::LOCATION,
        DirectiveSpec::leaf_value::<T>(name, vec![context::LOCATION], merge),
    );
}

fn finalize_location(
    node: &mut ConfigNode,
    _options: &BuildOptions<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pattern = node
        .payload::<Pattern>()?
        .expect("location node should contain a pattern payload");
    let proxy_pass = node.get::<ProxyPass>("proxy_pass")?;
    let has_cert = node.get::<PathConfig>("proxy_ssl_certificate")?.is_some();
    let has_key = node
        .get::<PathConfig>("proxy_ssl_certificate_key")?
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
        pattern::Pattern,
        tests::{
            build_proxy_conf, build_server_conf, cleanup_temp_files, create_temp_file,
            first_server, parse_doc,
        },
        types::{BoolConfig, DefaultType, HeaderRules, PathConfig, ProxyPass, StringList},
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
                .require::<PathConfig>("root")
                .expect("root should be typed")
                .0,
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
                .require::<PathConfig>("alias")
                .expect("alias should be typed")
                .0,
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
}

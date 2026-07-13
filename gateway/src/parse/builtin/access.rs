use std::path::{Path, PathBuf};

use snafu::{ResultExt, Snafu};

use crate::parse::{
    builtin::core::{first_arg_span, only_arg},
    normalize,
    registry::{DirectiveInput, DirectiveValue},
    source::SourceSpan,
    types::{AccessRulesUri, AccessRulesUriValidationError},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum AccessRulesUriError {
    #[snafu(display("invalid access_rules uri directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid access_rules uri directive value"))]
    Uri {
        span: SourceSpan,
        source: url::ParseError,
    },
    #[snafu(display("failed to resolve relative access_rules sqlite path"))]
    ResolveRelativePath {
        span: SourceSpan,
        source: normalize::NormalizeDirectiveValueError,
    },
    #[snafu(display("failed to encode the access_rules configuration base path as a URL"))]
    BasePathUrl { span: SourceSpan, path: PathBuf },
    #[snafu(display("invalid access_rules uri domain"))]
    Domain {
        span: SourceSpan,
        source: AccessRulesUriValidationError,
    },
}

impl DirectiveValue for AccessRulesUri {
    type Error = AccessRulesUriError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for AccessRulesUri {
    type Error = AccessRulesUriError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(AccessRulesUriError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let uri = url::Url::parse(&arg.value)
            .context(access_rules_uri_error::UriSnafu { span: arg.span })?;
        let path = AccessRulesUri::decoded_sqlite_path(&uri)
            .context(access_rules_uri_error::DomainSnafu { span: arg.span })?;
        if path.is_absolute() {
            return AccessRulesUri::try_from(uri)
                .context(access_rules_uri_error::DomainSnafu { span: arg.span });
        }

        let base_path = normalize::normalize_path(Path::new("."), arg.span, input.source_map)
            .context(access_rules_uri_error::ResolveRelativePathSnafu { span: arg.span })?;
        let base_url = url::Url::from_directory_path(&base_path).map_err(|()| {
            AccessRulesUriError::BasePathUrl {
                span: arg.span,
                path: base_path,
            }
        })?;
        let encoded_path = uri.path().to_owned();
        let query = uri.query().map(str::to_owned);
        let resolved = base_url
            .join(&encoded_path)
            .context(access_rules_uri_error::UriSnafu { span: arg.span })?;
        let mut uri = url::Url::parse("sqlite:///")
            .context(access_rules_uri_error::UriSnafu { span: arg.span })?;
        uri.set_path(resolved.path());
        uri.set_query(query.as_deref());
        AccessRulesUri::try_from(uri)
            .context(access_rules_uri_error::DomainSnafu { span: arg.span })
    }
}

#[cfg(test)]
mod tests {
    use crate::parse::tests::assert_error_chain_display_single_line;

    #[test]
    fn parse_access_rules_rejects_invalid_uri() {
        let conf = "pishoo { access_rules not-a-uri; }";

        let failure = crate::parse::parse_config_str_for_test(conf)
            .expect_err("invalid access_rules URI should fail");
        let report = snafu::Report::from_error(&failure.error).to_string();

        assert!(report.contains("failed to parse directive `access_rules`"));
        assert_error_chain_display_single_line(&failure.error);
    }

    #[tokio::test]
    async fn parse_access_rules_relative_sqlite_is_resolved_against_source_file() {
        let dir = std::env::temp_dir().join(format!(
            "gateway-relative-access-rules-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(dir.join("db")).expect("create db dir");
        std::fs::write(dir.join("server.crt"), "dummy cert").expect("write cert");
        std::fs::write(dir.join("server.key"), "dummy key").expect("write key");
        #[cfg(unix)]
        let access_rules = "sqlite:./db/access%3Fpart%23name%FF.db?mode=ro%23strict";
        #[cfg(not(unix))]
        let access_rules = "sqlite:./db/access%3Fpart%23name.db?mode=ro%23strict";
        std::fs::write(
            dir.join("server.conf"),
            format!(
                "server {{\n    listen all 5378;\n    server_name example.com;\n    ssl_certificate ./server.crt;\n    ssl_certificate_key ./server.key;\n    access_rules {access_rules};\n}}\n"
            ),
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
        #[cfg(unix)]
        let expected_suffix = "db/access%3Fpart%23name%FF.db?mode=ro%23strict";
        #[cfg(not(unix))]
        let expected_suffix = "db/access%3Fpart%23name.db?mode=ro%23strict";
        assert_eq!(
            server
                .require::<crate::parse::types::AccessRulesUri>("access_rules")
                .expect("access_rules should be typed")
                .0
                .as_str(),
            format!("sqlite://{}/{expected_suffix}", dir.display())
        );
    }

    #[test]
    fn parse_access_rules_rejects_non_sqlite_scheme() {
        let conf = "pishoo { access_rules https://example.com/rules.db; }";
        let failure = crate::parse::parse_config_str_for_test(conf)
            .expect_err("non-sqlite access_rules must fail");
        let report = snafu::Report::from_error(&failure.error).to_string();

        assert!(report.contains("unsupported access_rules uri scheme"));
    }

    #[test]
    fn parse_access_rules_rejects_sqlite_authority_form() {
        let conf = "pishoo { access_rules sqlite://localhost/tmp/rules.db; }";
        let failure = crate::parse::parse_config_str_for_test(conf)
            .expect_err("authority-form sqlite URI must fail");
        let report = snafu::Report::from_error(&failure.error).to_string();

        assert!(report.contains("unsupported sqlite access_rules uri form"));
    }
}

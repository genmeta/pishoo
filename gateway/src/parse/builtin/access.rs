use std::path::PathBuf;

use snafu::{ResultExt, Snafu, ensure};

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
    #[snafu(display("unsupported access_rules uri scheme `{scheme}`"))]
    UnsupportedScheme { span: SourceSpan, scheme: String },
    #[snafu(display("unsupported sqlite access_rules uri form"))]
    UnsupportedSqliteForm { span: SourceSpan },
    #[snafu(display("failed to resolve relative access_rules sqlite path"))]
    ResolveRelativePath {
        span: SourceSpan,
        source: normalize::NormalizeDirectiveValueError,
    },
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
        ensure!(
            uri.scheme() == "sqlite",
            access_rules_uri_error::UnsupportedSchemeSnafu {
                span: arg.span,
                scheme: uri.scheme().to_owned(),
            }
        );
        ensure!(
            uri.host_str().is_none()
                && uri.username().is_empty()
                && uri.password().is_none()
                && uri.port().is_none()
                && uri.fragment().is_none(),
            access_rules_uri_error::UnsupportedSqliteFormSnafu { span: arg.span }
        );

        let path = PathBuf::from(uri.path());
        let normalized_path = normalize::normalize_path(&path, arg.span, input.source_map)
            .context(access_rules_uri_error::ResolveRelativePathSnafu { span: arg.span })?;

        let mut normalized = format!("sqlite://{}", normalized_path.display());
        if let Some(query) = uri.query() {
            normalized.push('?');
            normalized.push_str(query);
        }

        let uri = url::Url::parse(&normalized)
            .context(access_rules_uri_error::UriSnafu { span: arg.span })?;
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
        std::fs::write(
            dir.join("server.conf"),
            "server {\n    listen all 5378;\n    server_name example.com;\n    ssl_certificate ./server.crt;\n    ssl_certificate_key ./server.key;\n    access_rules sqlite:./db/access.db?mode=ro;\n}\n",
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
        assert_eq!(
            server
                .require::<crate::parse::types::AccessRulesUri>("access_rules")
                .expect("access_rules should be typed")
                .0
                .as_str(),
            format!("sqlite://{}?mode=ro", dir.join("db/access.db").display())
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

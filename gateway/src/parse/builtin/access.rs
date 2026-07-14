use std::path::{Path, PathBuf};

use snafu::{ResultExt, Snafu};

use crate::parse::{
    builtin::core::{first_arg_span, only_arg},
    decode::{DirectiveInput, DirectiveValue},
    normalize,
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
    #[snafu(display("failed to encode the access_rules sqlite path as a URL"))]
    PathUrl { span: SourceSpan, path: PathBuf },
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
        let use_empty_authority = !path.is_absolute() || uri.as_str().starts_with("sqlite://");
        let path = normalize::normalize_path(&path, arg.span, input.source_map)
            .context(access_rules_uri_error::ResolveRelativePathSnafu { span: arg.span })?;
        let query = uri.query().map(str::to_owned);
        let uri = encode_sqlite_path(&path, query.as_deref(), use_empty_authority, arg.span)?;
        AccessRulesUri::try_from(uri)
            .context(access_rules_uri_error::DomainSnafu { span: arg.span })
    }
}

fn encode_sqlite_path(
    path: &Path,
    query: Option<&str>,
    use_empty_authority: bool,
    span: SourceSpan,
) -> Result<url::Url, AccessRulesUriError> {
    let path_url = url::Url::from_file_path(path).map_err(|()| AccessRulesUriError::PathUrl {
        span,
        path: path.to_owned(),
    })?;
    if path_url.host_str().is_some() {
        return Err(AccessRulesUriError::PathUrl {
            span,
            path: path.to_owned(),
        });
    }
    let root = if use_empty_authority {
        "sqlite:///"
    } else {
        "sqlite:/"
    };
    let mut uri = url::Url::parse(root).context(access_rules_uri_error::UriSnafu { span })?;
    uri.set_path(path_url.path());
    uri.set_query(query);
    Ok(uri)
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
    async fn parse_access_rules_relative_sqlite_preserves_os_path_bytes() {
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
        let mut cases = vec![("sqlite:a:b.db?mode=ro%23strict", dir.join("a:b.db"))];
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            cases.extend([
                (r"sqlite:a\b.db?mode=ro%23strict", dir.join(r"a\b.db")),
                ("sqlite:a%5Cb.db?mode=ro%23strict", dir.join(r"a\b.db")),
                (
                    "sqlite:a%3Fb%23c%FF.db?mode=ro%23strict",
                    dir.join(std::ffi::OsString::from_vec(b"a?b#c\xff.db".to_vec())),
                ),
            ]);
        }
        #[cfg(not(unix))]
        cases.push(("sqlite:a%3Fb%23c.db?mode=ro%23strict", dir.join("a?b#c.db")));
        let servers = cases
            .iter()
            .enumerate()
            .map(|(index, (access_rules, _))| {
                format!(
                    "server {{\n    listen all {};\n    server_name example-{index}.com;\n    ssl_certificate ./server.crt;\n    ssl_certificate_key ./server.key;\n    access_rules {access_rules};\n}}\n",
                    5378 + index,
                )
            })
            .collect::<String>();
        let config = format!("pishoo {{ {servers} }}");
        std::fs::write(dir.join("server.conf"), config).expect("write config");

        let text = std::fs::read_to_string(dir.join("server.conf")).unwrap();
        let parsed = crate::parse::TypedConfigParser::new()
            .parse_root(&text, &dir.join("server.conf"), None)
            .expect("config should load");

        assert_eq!(parsed.servers().len(), cases.len());
        for (server, (_, expected_path)) in parsed.servers().iter().zip(cases) {
            let uri = &server
                .result()
                .as_ref()
                .unwrap()
                .http()
                .access_rules()
                .effective()
                .as_ref()
                .unwrap()
                .0;
            assert_eq!(
                crate::parse::types::AccessRulesUri::decoded_sqlite_path(uri)
                    .expect("access_rules path should decode"),
                expected_path,
            );
            assert_eq!(uri.query(), Some("mode=ro%23strict"));
        }
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

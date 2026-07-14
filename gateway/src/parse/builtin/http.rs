use http::{HeaderName, HeaderValue, Uri};
use snafu::{OptionExt, ResultExt, Snafu, ensure};

use crate::parse::{
    builtin::core::{block_children, first_arg_span, only_arg},
    decode::{DirectiveInput, DirectiveValue},
    source::SourceSpan,
    types::{
        DefaultType, GzipCompLevel, GzipMinLength, HeaderRule, HeaderRules, MimeTypes,
        MimeTypesValidationError, ProxyPass,
    },
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum DefaultTypeError {
    #[snafu(display("invalid default_type directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid default_type directive value"))]
    HeaderValue {
        span: SourceSpan,
        source: http::header::InvalidHeaderValue,
    },
}

impl DirectiveValue for DefaultType {
    type Error = DefaultTypeError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for DefaultType {
    type Error = DefaultTypeError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(DefaultTypeError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let value = DefaultType::checked_from_bytes(arg.value.as_bytes())
            .context(default_type_error::HeaderValueSnafu { span: arg.span })?;
        Ok(value)
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum GzipMinLengthError {
    #[snafu(display("invalid gzip_min_length directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid gzip_min_length directive value"))]
    UnsignedInteger {
        span: SourceSpan,
        source: std::num::ParseIntError,
    },
}

impl DirectiveValue for GzipMinLength {
    type Error = GzipMinLengthError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for GzipMinLength {
    type Error = GzipMinLengthError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(GzipMinLengthError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let value = arg
            .value
            .parse::<u64>()
            .context(gzip_min_length_error::UnsignedIntegerSnafu { span: arg.span })?;
        Ok(Self::checked(value))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum GzipCompLevelError {
    #[snafu(display("invalid gzip_comp_level directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid gzip_comp_level directive value"))]
    SignedInteger {
        span: SourceSpan,
        source: std::num::ParseIntError,
    },
}

impl DirectiveValue for GzipCompLevel {
    type Error = GzipCompLevelError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for GzipCompLevel {
    type Error = GzipCompLevelError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(GzipCompLevelError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let value = arg
            .value
            .parse::<i32>()
            .context(gzip_comp_level_error::SignedIntegerSnafu { span: arg.span })?;
        Ok(Self::checked(value))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ProxyPassError {
    #[snafu(display("invalid proxy_pass directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid proxy_pass uri directive value"))]
    Uri {
        span: SourceSpan,
        source: http::uri::InvalidUri,
    },
    #[snafu(display("missing proxy_pass uri scheme"))]
    MissingScheme { span: SourceSpan },
    #[snafu(display("unsupported proxy_pass uri scheme `{scheme}`"))]
    UnsupportedScheme { span: SourceSpan, scheme: String },
    #[snafu(display("missing proxy_pass uri host"))]
    MissingHost { span: SourceSpan },
}

impl DirectiveValue for ProxyPass {
    type Error = ProxyPassError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for ProxyPass {
    type Error = ProxyPassError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(ProxyPassError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        build_proxy_pass(&arg.value, arg.span)
    }
}

fn build_proxy_pass(raw: &str, span: SourceSpan) -> Result<ProxyPass, ProxyPassError> {
    let uri = raw
        .parse::<Uri>()
        .context(proxy_pass_error::UriSnafu { span })?;
    let scheme = uri
        .scheme_str()
        .context(proxy_pass_error::MissingSchemeSnafu { span })?;
    ensure!(
        matches!(scheme, "http" | "https"),
        proxy_pass_error::UnsupportedSchemeSnafu {
            span,
            scheme: scheme.to_owned(),
        }
    );

    let authority = uri
        .authority()
        .context(proxy_pass_error::MissingHostSnafu { span })?;
    let proxy_host = authority.as_str().to_owned();

    let explicit_path_and_query = split_proxy_pass_suffix(raw)
        .map(str::to_owned)
        .filter(|suffix| !suffix.is_empty());

    Ok(ProxyPass {
        raw: raw.to_owned(),
        uri,
        proxy_host,
        explicit_path_and_query,
    })
}

fn split_proxy_pass_suffix(raw: &str) -> Option<&str> {
    let scheme_end = raw.find("://")? + 3;
    let suffix_start = raw[scheme_end..]
        .find(['/', '?'])
        .map(|index| scheme_end + index)?;
    raw.get(suffix_start..)
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum HeaderRulesError {
    #[snafu(display("invalid header directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid add_header always marker"))]
    InvalidAlways { span: SourceSpan, value: String },
    #[snafu(display("invalid header directive name"))]
    HeaderName {
        span: SourceSpan,
        source: http::header::InvalidHeaderName,
    },
    #[snafu(display("invalid header directive value"))]
    HeaderValue {
        span: SourceSpan,
        source: http::header::InvalidHeaderValue,
    },
}

impl DirectiveValue for HeaderRules {
    type Error = HeaderRulesError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for HeaderRules {
    type Error = HeaderRulesError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let args = &input.directive.args;
        let allow_always = input.directive.name.value == "add_header";
        let always = match args.as_slice() {
            [_, _] => !allow_always,
            [_, _, marker] if allow_always && marker.value == "always" => true,
            [_, _, marker] if allow_always => {
                return Err(HeaderRulesError::InvalidAlways {
                    span: marker.span,
                    value: marker.value.clone(),
                });
            }
            _ => {
                return Err(HeaderRulesError::InvalidArgumentCount {
                    span: input.directive.span,
                    expected: if allow_always { "2 or 3" } else { "2" },
                    actual: args.len(),
                });
            }
        };
        let name = HeaderName::from_bytes(args[0].value.as_bytes())
            .context(header_rules_error::HeaderNameSnafu { span: args[0].span })?;
        let value = HeaderValue::from_bytes(args[1].value.as_bytes())
            .context(header_rules_error::HeaderValueSnafu { span: args[1].span })?;
        Ok(Self(vec![HeaderRule {
            name,
            value,
            always,
        }]))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum MimeTypesError {
    #[snafu(display("types directive must be a block"))]
    InvalidBlock { span: SourceSpan },
    #[snafu(display("invalid types directive entries"))]
    Entries {
        span: SourceSpan,
        source: MimeTypesValidationError,
    },
}

impl DirectiveValue for MimeTypes {
    type Error = MimeTypesError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for MimeTypes {
    type Error = MimeTypesError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(children) = block_children(input) else {
            return Err(MimeTypesError::InvalidBlock {
                span: input.directive.span,
            });
        };
        let mut entries = Vec::new();
        for directive in children {
            for arg in &directive.args {
                entries.push((arg.value.clone(), directive.name.value.as_bytes().to_vec()));
            }
        }
        MimeTypes::checked_from_bytes(entries).context(mime_types_error::EntriesSnafu {
            span: input.directive.span,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::parse::tests::{
        assert_error_chain_display_single_line, build_proxy_conf, build_server_conf,
        cleanup_temp_files, create_temp_file, first_location, parse_root,
    };

    #[test]
    fn parse_default_type_accepts_single_header_value() {
        let conf = "pishoo { default_type text/html; }";

        let parsed = parse_root(conf).unwrap();
        let value = parsed
            .pishoo()
            .http()
            .default_type()
            .effective()
            .as_ref()
            .unwrap()
            .0
            .clone();

        assert_eq!(
            value.to_str().expect("default_type should be valid"),
            "text/html"
        );
    }

    #[test]
    fn parse_default_type_rejects_invalid_argument_count() {
        let conf = "pishoo { default_type text/plain text/html; }";

        let failure = crate::parse::parse_config_str_for_test(conf)
            .expect_err("default_type requires one arg");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid default_type directive argument count")
        );
    }

    #[test]
    fn parse_numeric_and_identity_directives_keep_typed_values() {
        let conf = "pishoo { gzip_min_length 1100; gzip_comp_level 6; }";
        let parsed = parse_root(conf).unwrap();
        let pishoo = parsed.pishoo();

        assert_eq!(pishoo.http().gzip_min_length().effective().0, 1100);
        assert_eq!(pishoo.http().gzip_comp_level().effective().0, 6);
    }

    #[test]
    fn parse_gzip_numeric_directives_reject_invalid_values() {
        let cert = create_temp_file("gzip_numeric_http_cert");
        let key = create_temp_file("gzip_numeric_http_key");
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
    fn parse_proxy_pass_preserves_no_explicit_uri() {
        let cert = create_temp_file("proxy_pass_no_uri_cert");
        let key = create_temp_file("proxy_pass_no_uri_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "location /api { proxy_pass http://backend.example.com; }",
        );

        let location = first_location(&conf).unwrap();
        let proxy_pass = location.proxy_pass().unwrap();

        assert_eq!(proxy_pass.proxy_host, "backend.example.com");
        assert!(!proxy_pass.has_explicit_uri());
        assert_eq!(proxy_pass.explicit_path_and_query(), None);

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_proxy_pass_preserves_explicit_root_uri() {
        let cert = create_temp_file("proxy_pass_root_uri_cert");
        let key = create_temp_file("proxy_pass_root_uri_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "location /api { proxy_pass http://backend.example.com/; }",
        );

        let location = first_location(&conf).unwrap();
        let proxy_pass = location.proxy_pass().unwrap();

        assert!(proxy_pass.has_explicit_uri());
        assert_eq!(proxy_pass.explicit_path_and_query(), Some("/"));

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_proxy_pass_preserves_explicit_nested_uri_and_port() {
        let cert = create_temp_file("proxy_pass_nested_uri_cert");
        let key = create_temp_file("proxy_pass_nested_uri_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "location /api { proxy_pass https://backend.example.com:8443/base/?v=1; }",
        );

        let location = first_location(&conf).unwrap();
        let proxy_pass = location.proxy_pass().unwrap();

        assert_eq!(proxy_pass.proxy_host, "backend.example.com:8443");
        assert!(proxy_pass.has_explicit_uri());
        assert_eq!(proxy_pass.explicit_path_and_query(), Some("/base/?v=1"));
        assert_eq!(proxy_pass.uri.port_u16(), Some(8443));

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_proxy_pass_accepts_http_and_https() {
        let cert = create_temp_file("proxy_pass_http_cert");
        let key = create_temp_file("proxy_pass_http_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "location /api { proxy_pass http://backend.example.com; }",
        );

        let location = first_location(&conf).unwrap();

        assert_eq!(location.proxy_pass().unwrap().scheme_str(), "http");

        let conf_https = build_server_conf(
            &cert,
            &key,
            "location /api { proxy_pass https://backend.example.com; }",
        );
        let location_https = first_location(&conf_https).unwrap();
        assert_eq!(location_https.proxy_pass().unwrap().scheme_str(), "https");

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_proxy_pass_rejects_missing_host() {
        let cert = create_temp_file("proxy_pass_missing_host_cert");
        let key = create_temp_file("proxy_pass_missing_host_key");
        let conf = build_server_conf(&cert, &key, "location /api { proxy_pass http:///path; }");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("missing proxy_pass host should fail");

        let report = snafu::Report::from_error(&failure.error).to_string();
        assert!(
            report.contains("missing proxy_pass host")
                || report.contains("invalid proxy_pass uri directive value")
                || report.contains("failed to parse directive `proxy_pass`")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_proxy_pass_rejects_unsupported_scheme() {
        let cert = create_temp_file("proxy_scheme_cert");
        let key = create_temp_file("proxy_scheme_key");
        let conf = build_proxy_conf(&cert, &key, "proxy_pass ftp://backend.example.com;");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("unsupported scheme should fail");
        let report = snafu::Report::from_error(&failure.error).to_string();

        assert!(report.contains("unsupported proxy_pass uri scheme"));
        assert_error_chain_display_single_line(&failure.error);

        cleanup_temp_files(&[&cert, &key]);
    }
}

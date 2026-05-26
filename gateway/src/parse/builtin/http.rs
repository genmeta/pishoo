use std::collections::HashMap;

use http::{HeaderName, HeaderValue, Uri};
use snafu::{OptionExt, ResultExt, Snafu, ensure};

use crate::parse::{
    builtin::core::{block_children, first_arg_span, only_arg},
    registry::{DirectiveInput, DirectiveValue},
    source::SourceSpan,
    types::{
        DefaultType, GzipCompLevel, GzipMinLength, HeaderRule, HeaderRules, MimeTypes, ProxyPass,
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
        let value = HeaderValue::from_str(&arg.value)
            .context(default_type_error::HeaderValueSnafu { span: arg.span })?;
        Ok(Self(value))
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
        Ok(Self(value))
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
        Ok(Self(value))
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
        let uri = arg
            .value
            .parse::<Uri>()
            .context(proxy_pass_error::UriSnafu { span: arg.span })?;
        let scheme = uri
            .scheme_str()
            .context(proxy_pass_error::MissingSchemeSnafu { span: arg.span })?;
        ensure!(
            matches!(scheme, "http" | "https"),
            proxy_pass_error::UnsupportedSchemeSnafu {
                span: arg.span,
                scheme: scheme.to_owned()
            }
        );
        uri.host()
            .context(proxy_pass_error::MissingHostSnafu { span: arg.span })?;
        Ok(Self(uri))
    }
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
    #[snafu(display("invalid MIME type header value"))]
    HeaderValue {
        span: SourceSpan,
        source: http::header::InvalidHeaderValue,
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
        let mut map = HashMap::new();
        for directive in children {
            let value = HeaderValue::from_str(&directive.name.value).context(
                mime_types_error::HeaderValueSnafu {
                    span: directive.name.span,
                },
            )?;
            for arg in &directive.args {
                map.insert(arg.value.clone(), value.clone());
            }
        }
        Ok(Self(map))
    }
}

#[cfg(test)]
mod tests {
    use crate::parse::{
        tests::{
            assert_error_chain_display_single_line, build_proxy_conf, build_server_conf,
            cleanup_temp_files, create_temp_file, first_pishoo, first_server, parse_doc,
        },
        types::{DefaultType, GzipCompLevel, GzipMinLength, ProxyPass},
    };

    #[test]
    fn parse_default_type_accepts_single_header_value() {
        let conf = "pishoo { default_type text/html; }";

        let pishoo = first_pishoo(&parse_doc(conf));
        let value = pishoo
            .require::<DefaultType>("default_type")
            .expect("default_type should be typed")
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
        let conf = "pishoo { gzip_min_length 1100; gzip_comp_level 6; proxy { listen 127.0.0.1:8080; client_name example.com; } }";

        let document = parse_doc(conf);
        let pishoo = first_pishoo(&document);

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
    fn parse_proxy_pass_accepts_http_and_https() {
        let cert = create_temp_file("proxy_pass_http_cert");
        let key = create_temp_file("proxy_pass_http_key");
        let conf = build_server_conf(
            &cert,
            &key,
            "location /api { proxy_pass http://backend.example.com; }",
        );

        let server = first_server(&parse_doc(&conf));
        let location = server.children("location").expect("location exists")[0].clone();

        assert_eq!(
            location
                .require::<ProxyPass>("proxy_pass")
                .expect("proxy_pass should be parsed")
                .0
                .scheme_str(),
            Some("http")
        );

        let conf_https = build_server_conf(
            &cert,
            &key,
            "location /api { proxy_pass https://backend.example.com; }",
        );
        let location_https = first_server(&parse_doc(&conf_https))
            .children("location")
            .expect("location exists")[0]
            .clone();
        assert_eq!(
            location_https
                .require::<ProxyPass>("proxy_pass")
                .expect("proxy_pass should be parsed")
                .0
                .scheme_str(),
            Some("https")
        );

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

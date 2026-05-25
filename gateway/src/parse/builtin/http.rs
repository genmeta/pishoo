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

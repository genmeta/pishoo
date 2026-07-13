use std::path::PathBuf;

use snafu::{Snafu, ensure};

use crate::parse::{
    ast::{AstBody, AstDirective, Spanned},
    registry::{DirectiveInput, DirectiveValue},
    source::SourceSpan,
    types::{BoolConfig, GzipTypesValidationError, PathConfig, StringConfig, StringList},
};

pub fn only_arg(directive: &AstDirective) -> Option<&Spanned<String>> {
    (directive.args.len() == 1).then(|| &directive.args[0])
}

pub fn first_arg_span(input: &DirectiveInput<'_>) -> SourceSpan {
    input
        .directive
        .args
        .first()
        .map(|arg| arg.span)
        .unwrap_or(input.directive.span)
}

pub fn block_children<'directive>(
    input: &DirectiveInput<'directive>,
) -> Option<&'directive [AstDirective]> {
    match &input.directive.body {
        AstBody::Block { children, .. } => Some(children),
        AstBody::Leaf { .. } => None,
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum StringConfigError {
    #[snafu(display("invalid string directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
}

impl DirectiveValue for StringConfig {
    type Error = StringConfigError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for StringConfig {
    type Error = StringConfigError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(StringConfigError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        Ok(Self(arg.value.clone()))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum StringListError {
    #[snafu(display("invalid gzip_types directive value"))]
    GzipTypes {
        span: SourceSpan,
        source: GzipTypesValidationError,
    },
}

impl DirectiveValue for StringList {
    type Error = StringListError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for StringList {
    type Error = StringListError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let values = input
            .directive
            .args
            .iter()
            .map(|arg| arg.value.clone())
            .collect();
        if input.directive.name.value == "gzip_types" {
            return StringList::checked_gzip_types(values).map_err(|source| {
                StringListError::GzipTypes {
                    span: input.directive.span,
                    source,
                }
            });
        }
        Ok(Self(values))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BoolConfigError {
    #[snafu(display("invalid boolean directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid boolean directive value"))]
    InvalidBoolean { span: SourceSpan, value: String },
}

impl DirectiveValue for BoolConfig {
    type Error = BoolConfigError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for BoolConfig {
    type Error = BoolConfigError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(BoolConfigError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let value = match arg.value.as_str() {
            "on" => true,
            "off" => false,
            _ => {
                return Err(BoolConfigError::InvalidBoolean {
                    span: arg.span,
                    value: arg.value.clone(),
                });
            }
        };
        Ok(Self(value))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum PathConfigError {
    #[snafu(display("invalid path directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
}

impl DirectiveValue for PathConfig {
    type Error = PathConfigError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for PathConfig {
    type Error = PathConfigError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(PathConfigError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        Ok(Self(PathBuf::from(&arg.value)))
    }
}

#[cfg(test)]
mod tests {
    use crate::parse::tests::*;

    #[test]
    fn parse_bool_directive_rejects_invalid_value() {
        let cert = create_temp_file("bool_invalid_cert");
        let key = create_temp_file("bool_invalid_key");
        let conf = build_server_conf(&cert, &key, "gzip yes;");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("invalid bool value should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid boolean directive value")
        );
        assert_error_chain_display_single_line(&failure.error);

        cleanup_temp_files(&[&cert, &key]);
    }
}

#[derive(Debug)]
pub struct ExistingPathConfig(pub PathConfig);

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ExistingPathConfigError {
    #[snafu(display("invalid existing path directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("configured path does not exist"))]
    MissingPath { span: SourceSpan, path: PathBuf },
}

impl DirectiveValue for ExistingPathConfig {
    type Error = ExistingPathConfigError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for ExistingPathConfig {
    type Error = ExistingPathConfigError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(ExistingPathConfigError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let path = PathBuf::from(&arg.value);
        ensure!(
            path.exists(),
            existing_path_config_error::MissingPathSnafu {
                span: arg.span,
                path
            }
        );
        Ok(Self(PathConfig(path)))
    }
}

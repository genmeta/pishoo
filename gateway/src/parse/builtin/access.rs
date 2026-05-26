use snafu::{ResultExt, Snafu};

use crate::parse::{
    builtin::core::{first_arg_span, only_arg},
    registry::{DirectiveInput, DirectiveValue},
    source::SourceSpan,
    types::AccessRulesUri,
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
        Ok(Self(uri))
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
}

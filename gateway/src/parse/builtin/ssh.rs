use snafu::Snafu;

use crate::parse::{
    registry::{DirectiveInput, DirectiveValue},
    source::SourceSpan,
    types::{SshLoginMethods, SshSslUser, SshSslUsers},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SshLoginMethodsError {
    #[snafu(display("missing ssh login method"))]
    MissingMethod { span: SourceSpan },
    #[snafu(display("invalid ssh login method `{method}`"))]
    InvalidMethod { span: SourceSpan, method: String },
}

impl DirectiveValue for SshLoginMethods {
    type Error = SshLoginMethodsError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for SshLoginMethods {
    type Error = SshLoginMethodsError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        if input.directive.args.is_empty() {
            return Err(SshLoginMethodsError::MissingMethod {
                span: input.directive.span,
            });
        }
        let mut methods = Vec::with_capacity(input.directive.args.len());
        for arg in &input.directive.args {
            if arg.value != "ssl" {
                return Err(SshLoginMethodsError::InvalidMethod {
                    span: arg.span,
                    method: arg.value.clone(),
                });
            }
            methods.push(arg.value.clone());
        }
        Ok(Self(methods))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SshSslUsersError {
    #[snafu(display("invalid ssh_ssl_user directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
}

impl DirectiveValue for SshSslUsers {
    type Error = SshSslUsersError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for SshSslUsers {
    type Error = SshSslUsersError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let args = &input.directive.args;
        if args.len() != 2 {
            return Err(SshSslUsersError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "2",
                actual: args.len(),
            });
        }
        Ok(Self(vec![SshSslUser {
            name: args[0].value.clone(),
            user: args[1].value.clone(),
        }]))
    }
}

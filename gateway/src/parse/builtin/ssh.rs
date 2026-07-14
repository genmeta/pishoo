use snafu::Snafu;

use crate::parse::{
    decode::{DirectiveInput, DirectiveValue},
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

#[cfg(test)]
mod tests {
    use crate::parse::tests::{
        build_proxy_conf, cleanup_temp_files, create_temp_file, first_location,
    };

    #[test]
    fn parse_ssh_ssl_users_keeps_name_user_tuple() {
        let cert = create_temp_file("ssh_ssl_user_cert");
        let key = create_temp_file("ssh_ssl_user_key");
        let conf = build_proxy_conf(&cert, &key, "ssh_ssl_user alice aliceroot;");

        let location = first_location(&conf).unwrap();
        let users = &location.ssh_users()[0].0;
        assert_eq!(users[0].name, "alice");
        assert_eq!(users[0].user, "aliceroot");

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_ssh_ssl_users_rejects_invalid_argument_count() {
        let cert = create_temp_file("ssh_ssl_user_invalid_cert");
        let key = create_temp_file("ssh_ssl_user_invalid_key");
        let conf = build_proxy_conf(&cert, &key, "ssh_ssl_user alice; ");

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("ssh_ssl_user requires two args");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid ssh_ssl_user directive argument count")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_ssh_login_rejects_invalid_method() {
        let cert = create_temp_file("ssh_login_invalid_cert");
        let key = create_temp_file("ssh_login_invalid_key");

        let missing_method = build_proxy_conf(&cert, &key, "ssh_login;");
        let missing = crate::parse::parse_config_str_for_test(&missing_method)
            .expect_err("missing ssh login method should fail");
        assert!(
            snafu::Report::from_error(&missing.error)
                .to_string()
                .contains("missing ssh login method")
        );

        let invalid_method = build_proxy_conf(&cert, &key, "ssh_login tls;");
        let invalid = crate::parse::parse_config_str_for_test(&invalid_method)
            .expect_err("invalid ssh login method should fail");
        assert!(
            snafu::Report::from_error(&invalid.error)
                .to_string()
                .contains("invalid ssh login method")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_ssh_login_keeps_semantic_type() {
        let cert = create_temp_file("ssh_login_cert");
        let key = create_temp_file("ssh_login_key");
        let conf = build_proxy_conf(&cert, &key, "ssh_login ssl;");

        let location = first_location(&conf).unwrap();

        assert_eq!(location.ssh_login().unwrap().0, vec!["ssl".to_owned()]);

        cleanup_temp_files(&[&cert, &key]);
    }
}

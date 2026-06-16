use std::net::SocketAddr;

use snafu::{ResultExt, Snafu};

use crate::parse::{
    builtin::core::{first_arg_span, only_arg},
    registry::{
        ConfigRegistry, DirectiveInput, DirectiveSpec, DirectiveValue, MergePolicy, context,
    },
    source::SourceSpan,
    types::{SocketAddrs, StunBindConfigValue, StunChangePort},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum StunBindConfigValueError {
    #[snafu(display("invalid stun_server bind directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid stun_server bind socket address directive value"))]
    SocketAddr {
        span: SourceSpan,
        source: std::net::AddrParseError,
    },
    #[snafu(display("invalid stun_server bind change_port directive value"))]
    Port {
        span: SourceSpan,
        source: std::num::ParseIntError,
    },
    #[snafu(display("invalid stun_server bind directive option"))]
    InvalidOption { span: SourceSpan },
}

impl DirectiveValue for StunBindConfigValue {
    type Error = StunBindConfigValueError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for StunBindConfigValue {
    type Error = StunBindConfigValueError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let args = &input.directive.args;
        if args.is_empty() {
            return Err(StunBindConfigValueError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "at least 1",
                actual: args.len(),
            });
        }
        let bind = args[0]
            .value
            .parse::<SocketAddr>()
            .context(stun_bind_config_value_error::SocketAddrSnafu { span: args[0].span })?;
        let mut value = Self {
            bind,
            outer_addr: None,
            change_addr: None,
            change_port: None,
        };
        let mut index = 1;
        while index < args.len() {
            match args[index].value.as_str() {
                "outer_addr" if index + 1 < args.len() => {
                    value.outer_addr = Some(args[index + 1].value.parse::<SocketAddr>().context(
                        stun_bind_config_value_error::SocketAddrSnafu {
                            span: args[index + 1].span,
                        },
                    )?);
                    index += 2;
                }
                "change_addr" if index + 1 < args.len() => {
                    value.change_addr = Some(args[index + 1].value.parse::<SocketAddr>().context(
                        stun_bind_config_value_error::SocketAddrSnafu {
                            span: args[index + 1].span,
                        },
                    )?);
                    index += 2;
                }
                "change_port" if index + 1 < args.len() => {
                    value.change_port = Some(args[index + 1].value.parse::<u16>().context(
                        stun_bind_config_value_error::PortSnafu {
                            span: args[index + 1].span,
                        },
                    )?);
                    index += 2;
                }
                _ => {
                    return Err(StunBindConfigValueError::InvalidOption {
                        span: args[index].span,
                    });
                }
            }
        }
        Ok(value)
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum StunChangePortError {
    #[snafu(display("invalid stun_server change_port directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid stun_server change_port directive value"))]
    Port {
        span: SourceSpan,
        source: std::num::ParseIntError,
    },
}

impl DirectiveValue for StunChangePort {
    type Error = StunChangePortError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for StunChangePort {
    type Error = StunChangePortError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(StunChangePortError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let port = arg
            .value
            .parse::<u16>()
            .context(stun_change_port_error::PortSnafu { span: arg.span })?;
        Ok(Self(port))
    }
}

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::STUN_SERVER,
        finalize: None,
    });
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::context_empty(
            "stun_server",
            vec![context::SERVER],
            context::STUN_SERVER,
            MergePolicy::Append,
        ),
    );
    registry.register_directive(
        context::STUN_SERVER,
        DirectiveSpec::leaf_value::<StunBindConfigValue>(
            "bind",
            vec![context::STUN_SERVER],
            MergePolicy::Append,
        ),
    );
    for name in ["outer_addr", "change_addr"] {
        registry.register_directive(
            context::STUN_SERVER,
            DirectiveSpec::leaf_value::<SocketAddrs>(
                name,
                vec![context::STUN_SERVER],
                MergePolicy::RejectDuplicate,
            ),
        );
    }
    registry.register_directive(
        context::STUN_SERVER,
        DirectiveSpec::leaf_value::<StunChangePort>(
            "change_port",
            vec![context::STUN_SERVER],
            MergePolicy::RejectDuplicate,
        ),
    );
}

#[cfg(test)]
mod tests {
    use crate::parse::{
        tests::{
            assert_error_chain_display_single_line, cleanup_temp_files, create_temp_file,
            first_server, parse_doc,
        },
        types::{SocketAddrs, StunBindConfigValue, StunChangePort},
    };

    #[test]
    fn parse_stun_bind_and_ports() {
        let cert = create_temp_file("stun_bind_cert");
        let key = create_temp_file("stun_bind_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; stun_server {{ bind 10.0.0.1:20000 outer_addr 10.0.0.2:20001 change_addr 10.0.0.3:20002 change_port 3478; }} }} }}",
            cert.display(),
            key.display()
        );

        let server = first_server(&parse_doc(&conf));
        let stun = server
            .children("stun_server")
            .expect("stun_server should exist")[0]
            .clone();
        let bind = stun.require::<StunBindConfigValue>("bind").unwrap();
        assert_eq!(
            bind.bind,
            "10.0.0.1:20000".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(
            bind.outer_addr.unwrap(),
            "10.0.0.2:20001".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(
            bind.change_addr.unwrap(),
            "10.0.0.3:20002".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(bind.change_port.unwrap(), 3478);

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_stun_bind_config_rejects_unknown_option() {
        let cert = create_temp_file("stun_bind_unknown_cert");
        let key = create_temp_file("stun_bind_unknown_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; stun_server {{ bind 10.0.0.1:20000 bad 10.0.0.1:20001; }} }} }}",
            cert.display(),
            key.display()
        );

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("unknown bind option should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("invalid stun_server bind directive option")
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_stun_change_port_accepts_valid_port() {
        let cert = create_temp_file("stun_port_valid_cert");
        let key = create_temp_file("stun_port_valid_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; stun_server {{ bind 127.0.0.1:20000; change_port 3478; }} }} }}",
            cert.display(),
            key.display()
        );

        let server = first_server(&parse_doc(&conf));
        let stun = server
            .children("stun_server")
            .expect("stun_server should exist")[0]
            .clone();
        assert_eq!(
            stun.require::<StunChangePort>("change_port").unwrap().0,
            3478
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_stun_server_accepts_outer_addr_and_change_addr() {
        let cert = create_temp_file("stun_addr_cert");
        let key = create_temp_file("stun_addr_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; stun_server {{ bind 127.0.0.1:20000; outer_addr 127.0.0.1:20001; change_addr 127.0.0.1:20002; }} }} }}",
            cert.display(),
            key.display()
        );

        let server = first_server(&parse_doc(&conf));
        let stun = server
            .children("stun_server")
            .expect("stun_server should exist")[0]
            .clone();

        assert_eq!(
            stun.require::<SocketAddrs>("outer_addr")
                .expect("outer_addr should be typed")
                .0,
            vec![
                "127.0.0.1:20001"
                    .parse::<std::net::SocketAddr>()
                    .expect("outer address should parse")
            ]
        );
        assert_eq!(
            stun.require::<SocketAddrs>("change_addr")
                .expect("change_addr should be typed")
                .0,
            vec![
                "127.0.0.1:20002"
                    .parse::<std::net::SocketAddr>()
                    .expect("change address should parse")
            ]
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_stun_change_port_rejects_invalid_value() {
        let cert = create_temp_file("stun_port_cert");
        let key = create_temp_file("stun_port_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; stun_server {{ bind 127.0.0.1:20000; change_port invalid; }} }} }}",
            cert.display(),
            key.display()
        );

        let failure = crate::parse::parse_config_str_for_test(&conf)
            .expect_err("invalid change_port should fail");
        let report = snafu::Report::from_error(&failure.error).to_string();

        assert!(report.contains("failed to parse directive `change_port`"));
        assert_error_chain_display_single_line(&failure.error);

        cleanup_temp_files(&[&cert, &key]);
    }
}

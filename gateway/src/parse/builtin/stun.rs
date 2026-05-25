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

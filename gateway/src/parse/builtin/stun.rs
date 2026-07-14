use std::net::SocketAddr;

use snafu::{ResultExt, Snafu};

use crate::parse::{
    ast::AstBody,
    builtin::{
        core::{first_arg_span, only_arg},
        net::SocketAddrsError,
    },
    registry::{
        CascadePolicy, ConfigRegistry, DirectiveInput, DirectiveValue, ReloadImpact,
        RepeatedCardinality, RepeatedDirectiveKey, TransportPolicy, TypedDirectiveDefinition,
        context,
    },
    source::SourceSpan,
    types::{SocketAddrs, StunBindConfigValue, StunChangePort, StunServerConfigValue},
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

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum StunServerConfigValueError {
    #[snafu(display("stun_server must use block form"))]
    ExpectedBlock { span: SourceSpan },
    #[snafu(display("unknown stun_server child directive `{name}`"))]
    UnknownChild { name: String, span: SourceSpan },
    #[snafu(display("stun_server child directive `{name}` must use leaf form"))]
    ExpectedLeaf { name: String, span: SourceSpan },
    #[snafu(display("duplicate stun_server fallback directive `{name}`"))]
    DuplicateFallback { name: String, span: SourceSpan },
    #[snafu(display("invalid stun_server bind directive"))]
    Bind { source: StunBindConfigValueError },
    #[snafu(display("invalid stun_server address fallback"))]
    Address { source: SocketAddrsError },
    #[snafu(display("invalid stun_server change_port fallback"))]
    ChangePort { source: StunChangePortError },
}

impl DirectiveValue for StunServerConfigValue {
    type Error = StunServerConfigValueError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for StunServerConfigValue {
    type Error = StunServerConfigValueError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let AstBody::Block { children, .. } = &input.directive.body else {
            return stun_server_config_value_error::ExpectedBlockSnafu {
                span: input.directive.span,
            }
            .fail();
        };
        let mut binds = Vec::new();
        let mut outer_addr = None;
        let mut change_addr = None;
        let mut change_port = None;
        for child in children {
            if !child.is_leaf() {
                return stun_server_config_value_error::ExpectedLeafSnafu {
                    name: child.name.value.clone(),
                    span: child.span,
                }
                .fail();
            }
            let child_input = DirectiveInput {
                directive: child,
                context: input.context,
                source_map: input.source_map,
            };
            match child.name.value.as_str() {
                "bind" => binds.push(
                    StunBindConfigValue::try_from(&child_input)
                        .context(stun_server_config_value_error::BindSnafu)?,
                ),
                "outer_addr" => {
                    if outer_addr.is_some() {
                        return stun_server_config_value_error::DuplicateFallbackSnafu {
                            name: child.name.value.clone(),
                            span: child.span,
                        }
                        .fail();
                    }
                    outer_addr = SocketAddrs::try_from(&child_input)
                        .context(stun_server_config_value_error::AddressSnafu)?
                        .0
                        .first()
                        .copied();
                }
                "change_addr" => {
                    if change_addr.is_some() {
                        return stun_server_config_value_error::DuplicateFallbackSnafu {
                            name: child.name.value.clone(),
                            span: child.span,
                        }
                        .fail();
                    }
                    change_addr = SocketAddrs::try_from(&child_input)
                        .context(stun_server_config_value_error::AddressSnafu)?
                        .0
                        .first()
                        .copied();
                }
                "change_port" => {
                    if change_port.is_some() {
                        return stun_server_config_value_error::DuplicateFallbackSnafu {
                            name: child.name.value.clone(),
                            span: child.span,
                        }
                        .fail();
                    }
                    change_port = Some(
                        StunChangePort::try_from(&child_input)
                            .context(stun_server_config_value_error::ChangePortSnafu)?
                            .0,
                    );
                }
                name => {
                    return stun_server_config_value_error::UnknownChildSnafu {
                        name: name.to_owned(),
                        span: child.name.span,
                    }
                    .fail();
                }
            }
        }
        for bind in &mut binds {
            bind.outer_addr = bind.outer_addr.or(outer_addr);
            bind.change_addr = bind.change_addr.or(change_addr);
            bind.change_port = bind.change_port.or(change_port);
        }
        Ok(Self {
            binds: binds.into_boxed_slice(),
        })
    }
}

const STUN_SERVERS_DEFINITION: TypedDirectiveDefinition<
    StunServerConfigValue,
    RepeatedCardinality,
> = TypedDirectiveDefinition::repeated_raw(
    context::SERVER,
    "stun_server",
    CascadePolicy::None,
    TransportPolicy::WorkerLocalOnly,
    ReloadImpact::ListenerSet,
);
pub(crate) const STUN_SERVERS_KEY: RepeatedDirectiveKey<StunServerConfigValue> =
    STUN_SERVERS_DEFINITION.key();

pub fn register(registry: &mut ConfigRegistry) {
    STUN_SERVERS_DEFINITION.register(registry);
}

#[cfg(test)]
mod tests {
    use crate::parse::{
        ConfigDocumentParser,
        domain::ConfigDocumentRole,
        fragment::ParsedConfigDocument,
        keys,
        tests::{cleanup_temp_files, create_temp_file},
        tree::build_global_tree,
    };

    fn sealed_server(stun: &str) -> crate::parse::tree::ServerConfigRef {
        let cert = create_temp_file("stun_compound_cert");
        let key = create_temp_file("stun_compound_key");
        let text = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; {stun} }} }}",
            cert.display(),
            key.display()
        );
        let registry = crate::parse::default_registry();
        let mut parser = ConfigDocumentParser::new(&registry);
        let ParsedConfigDocument::HypervisorRoot(root) = parser
            .parse_text(
                &text,
                std::path::Path::new("/tmp/pishoo.conf"),
                ConfigDocumentRole::HypervisorRoot { home: None },
            )
            .unwrap()
        else {
            panic!("expected root fragment")
        };
        let tree = build_global_tree(&registry, root, []).unwrap();
        let server = tree.servers().next().unwrap();
        cleanup_temp_files(&[&cert, &key]);
        server
    }

    #[test]
    fn stun_compounds_preserve_block_and_bind_order() {
        let server = sealed_server(
            "stun_server { bind 127.0.0.1:1000; bind 127.0.0.1:1001; } stun_server { bind 127.0.0.1:2000; }",
        );
        let values = server.node().repeated(keys::server::STUN_SERVERS).unwrap();
        let addresses = values
            .iter()
            .flat_map(|value| value.binds.iter().map(|bind| bind.bind))
            .collect::<Vec<_>>();
        assert_eq!(
            addresses,
            [
                "127.0.0.1:1000".parse().unwrap(),
                "127.0.0.1:1001".parse().unwrap(),
                "127.0.0.1:2000".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn stun_bind_explicit_values_override_block_fallbacks() {
        let server = sealed_server(
            "stun_server { outer_addr 127.0.0.1:3000; change_port 4000; bind 127.0.0.1:1000 outer_addr 127.0.0.1:3001 change_port 4001; }",
        );
        let values = server.node().repeated(keys::server::STUN_SERVERS).unwrap();
        let bind = &values[0].binds[0];
        assert_eq!(bind.outer_addr, Some("127.0.0.1:3001".parse().unwrap()));
        assert_eq!(bind.change_port, Some(4001));
    }

    #[test]
    fn empty_stun_server_block_remains_a_noop_compound() {
        let server = sealed_server("stun_server {}");
        let values = server.node().repeated(keys::server::STUN_SERVERS).unwrap();
        assert_eq!(values.len(), 1);
        assert!(values[0].binds.is_empty());
    }

    #[test]
    fn stun_server_leaf_form_is_rejected() {
        let failure = crate::parse::parse_config_str_for_test(
            "pishoo { server { listen all 1; server_name x; ssl_certificate /tmp/c; ssl_certificate_key /tmp/k; stun_server value; } }",
        )
        .unwrap_err();
        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("stun_server must use block form")
        );
    }
}

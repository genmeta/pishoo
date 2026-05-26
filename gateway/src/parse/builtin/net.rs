use std::net::SocketAddr;

use http::Uri;
use snafu::{ResultExt, Snafu};

use crate::parse::{
    builtin::core::{first_arg_span, only_arg},
    registry::{DirectiveInput, DirectiveValue},
    source::SourceSpan,
    types::{
        ClientNameConfig, IfaceRange, IpFamilies, ListenConfig, Listens, ResolverConfig,
        ServerIdConfig, ServerName, ServerNames, SocketAddrs,
    },
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ResolverConfigError {
    #[snafu(display("invalid dns directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid dns resolver uri directive value"))]
    Uri {
        span: SourceSpan,
        source: http::uri::InvalidUri,
    },
    #[snafu(display("deprecated resolver kind `{kind}`"))]
    DeprecatedResolver { span: SourceSpan, kind: String },
    #[snafu(display("unsupported resolver kind `{kind}`"))]
    UnsupportedResolver { span: SourceSpan, kind: String },
}

impl DirectiveValue for ResolverConfig {
    type Error = ResolverConfigError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for ResolverConfig {
    type Error = ResolverConfigError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let args = &input.directive.args;
        if args.len() != 2 {
            return Err(ResolverConfigError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "2",
                actual: args.len(),
            });
        }
        match args[0].value.as_str() {
            "h3" => {
                let uri = args[1]
                    .value
                    .parse::<Uri>()
                    .context(resolver_config_error::UriSnafu { span: args[1].span })?;
                Ok(Self(uri))
            }
            "udp" | "http" => Err(ResolverConfigError::DeprecatedResolver {
                span: args[0].span,
                kind: args[0].value.clone(),
            }),
            kind => Err(ResolverConfigError::UnsupportedResolver {
                span: args[0].span,
                kind: kind.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ServerIdConfigError {
    #[snafu(display("invalid server_id directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid server_id directive value"))]
    Port {
        span: SourceSpan,
        source: std::num::ParseIntError,
    },
}

impl DirectiveValue for ServerIdConfig {
    type Error = ServerIdConfigError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for ServerIdConfig {
    type Error = ServerIdConfigError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(ServerIdConfigError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let id = arg
            .value
            .parse::<u8>()
            .context(server_id_config_error::PortSnafu { span: arg.span })?;
        Ok(Self(id))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ServerNamesError {
    #[snafu(display("invalid server_name directive value"))]
    ServerName {
        span: SourceSpan,
        source: dhttp::name::InvalidDhttpName,
    },
}

impl DirectiveValue for ServerNames {
    type Error = ServerNamesError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for ServerNames {
    type Error = ServerNamesError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let mut names = Vec::with_capacity(input.directive.args.len());
        for arg in &input.directive.args {
            let name = dhttp::name::DhttpName::try_from(arg.value.clone())
                .context(server_names_error::ServerNameSnafu { span: arg.span })?;
            names.push(ServerName { name });
        }
        Ok(Self(names))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ClientNameConfigError {
    #[snafu(display("invalid client_name directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid client_name directive value"))]
    ClientName {
        span: SourceSpan,
        source: dhttp::name::InvalidDhttpName,
    },
}

impl DirectiveValue for ClientNameConfig {
    type Error = ClientNameConfigError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for ClientNameConfig {
    type Error = ClientNameConfigError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(ClientNameConfigError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        let name = dhttp::name::DhttpName::try_from(arg.value.clone())
            .context(client_name_config_error::ClientNameSnafu { span: arg.span })?;
        Ok(Self(name))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ListenConfigError {
    #[snafu(display("invalid listen directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid listen socket address directive value"))]
    SocketAddr {
        span: SourceSpan,
        source: std::net::AddrParseError,
    },
    #[snafu(display("invalid listen port directive value"))]
    Port {
        span: SourceSpan,
        source: std::num::ParseIntError,
    },
    #[snafu(display("invalid listen ip family directive value `{value}`"))]
    IpFamilies { span: SourceSpan, value: String },
}

impl DirectiveValue for ListenConfig {
    type Error = ListenConfigError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for ListenConfig {
    type Error = ListenConfigError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let args = &input.directive.args;
        let listen = match args.as_slice() {
            [iface] => {
                if iface.value.contains(',') || iface.value.parse::<SocketAddr>().is_ok() {
                    let addrs = listen_socket_addrs_from_value(&iface.value, iface.span)?;
                    Listens {
                        range: IfaceRange::All,
                        families: IpFamilies::Dual,
                        port: 0,
                        specific_addrs: Some(addrs),
                    }
                } else {
                    Listens {
                        range: IfaceRange::from(iface.value.as_str()),
                        families: IpFamilies::default(),
                        port: 0,
                        specific_addrs: None,
                    }
                }
            }
            [iface, param] => {
                let range = IfaceRange::from(iface.value.as_str());
                match ip_families_from_value(&param.value, param.span) {
                    Ok(families) => Listens {
                        range,
                        families,
                        port: 0,
                        specific_addrs: None,
                    },
                    Err(_) => {
                        let port = param
                            .value
                            .parse::<u16>()
                            .context(listen_config_error::PortSnafu { span: param.span })?;
                        Listens {
                            range,
                            families: IpFamilies::default(),
                            port,
                            specific_addrs: None,
                        }
                    }
                }
            }
            [iface, version, port] => {
                let families = ip_families_from_value(&version.value, version.span)?;
                let port = port
                    .value
                    .parse::<u16>()
                    .context(listen_config_error::PortSnafu { span: port.span })?;
                Listens {
                    range: IfaceRange::from(iface.value.as_str()),
                    families,
                    port,
                    specific_addrs: None,
                }
            }
            _ => {
                return Err(ListenConfigError::InvalidArgumentCount {
                    span: input.directive.span,
                    expected: "1, 2, or 3",
                    actual: args.len(),
                });
            }
        };
        Ok(Self(vec![listen]))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SocketAddrsError {
    #[snafu(display("invalid socket address directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid socket address directive value"))]
    SocketAddr {
        span: SourceSpan,
        source: std::net::AddrParseError,
    },
}

impl DirectiveValue for SocketAddrs {
    type Error = SocketAddrsError;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        first_arg_span(input)
    }
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for SocketAddrs {
    type Error = SocketAddrsError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        let Some(arg) = only_arg(input.directive) else {
            return Err(SocketAddrsError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1",
                actual: input.directive.args.len(),
            });
        };
        Ok(Self(socket_addrs_from_value(&arg.value, arg.span)?))
    }
}

fn listen_socket_addrs_from_value(
    value: &str,
    span: SourceSpan,
) -> Result<Vec<SocketAddr>, ListenConfigError> {
    value
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<SocketAddr>()
                .context(listen_config_error::SocketAddrSnafu { span })
        })
        .collect()
}

fn socket_addrs_from_value(
    value: &str,
    span: SourceSpan,
) -> Result<Vec<SocketAddr>, SocketAddrsError> {
    value
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<SocketAddr>()
                .context(socket_addrs_error::SocketAddrSnafu { span })
        })
        .collect()
}

fn ip_families_from_value(value: &str, span: SourceSpan) -> Result<IpFamilies, ListenConfigError> {
    match value {
        "v4only" => Ok(IpFamilies::V4),
        "v6only" => Ok(IpFamilies::V6),
        "dual" => Ok(IpFamilies::Dual),
        _ => Err(ListenConfigError::IpFamilies {
            span,
            value: value.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use crate::parse::{tests::parse_doc, types::SocketAddrs};

    #[test]
    fn parse_socket_addrs_keeps_multiple_addresses() {
        let conf =
            "pishoo { proxy { listen 127.0.0.1:8080,127.0.0.2:8081; allow any; deny none; } }";

        let document = parse_doc(conf);
        let pishoo = document.root.children("pishoo").expect("pishoo children")[0].clone();
        let proxy = pishoo.children("proxy").expect("proxy should exist")[0].clone();

        assert_eq!(
            proxy.require::<SocketAddrs>("listen").unwrap().0,
            vec![
                "127.0.0.1:8080"
                    .parse()
                    .expect("first address should parse"),
                "127.0.0.2:8081"
                    .parse()
                    .expect("second address should parse"),
            ]
        );
    }

    #[test]
    fn parse_socket_addrs_rejects_invalid_address() {
        let conf = "pishoo { proxy { listen not-a-socket; } }";

        let failure = crate::parse::parse_config_str_for_test(conf)
            .expect_err("invalid socket addr should fail");

        assert!(
            snafu::Report::from_error(&failure.error)
                .to_string()
                .contains("failed to parse directive `listen`")
        );
    }

    #[test]
    fn parse_address_directives_keep_socket_addrs_type() {
        let conf = "pishoo { proxy { listen 127.0.0.1:8080; } }";

        let document = parse_doc(conf);
        let pishoo = document.root.children("pishoo").expect("pishoo children")[0].clone();
        let proxy = pishoo.children("proxy").expect("proxy should exist")[0].clone();

        assert_eq!(
            proxy
                .require::<SocketAddrs>("listen")
                .expect("proxy listen should be typed")
                .0,
            vec!["127.0.0.1:8080".parse().expect("address should parse")]
        );
    }
}

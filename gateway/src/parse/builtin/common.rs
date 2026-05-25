use std::{collections::HashMap, net::SocketAddr, path::PathBuf, str::FromStr};

use http::{HeaderName, HeaderValue, Uri};
use snafu::{OptionExt, ResultExt, Snafu, ensure};

use crate::parse::{
    ast::{AstBody, AstDirective, Spanned},
    registry::{DirectiveInput, ParsedDirective},
    source::SourceSpan,
    types::{
        BoolConfig, DefaultType, HeaderRule, HeaderRules, IfaceRange, IpFamilies, ListenConfig,
        Listens, MimeTypes, PathConfig, ProxyPass, ResolverConfig, ServerIdConfig, ServerName,
        ServerNames, SshSslUser, SshSslUsers, StringConfig, StringList, StunBindConfigValue,
    },
    value::TypedValue,
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ParseDirectiveValueError {
    #[snafu(display("invalid boolean directive value"))]
    InvalidBoolean { span: SourceSpan, value: String },
    #[snafu(display("invalid directive argument count"))]
    InvalidArgumentCount {
        span: SourceSpan,
        expected: &'static str,
        actual: usize,
    },
    #[snafu(display("invalid uri directive value"))]
    Uri {
        span: SourceSpan,
        source: http::uri::InvalidUri,
    },
    #[snafu(display("missing proxy_pass uri scheme"))]
    MissingProxyPassScheme { span: SourceSpan },
    #[snafu(display("unsupported proxy_pass uri scheme `{scheme}`"))]
    UnsupportedProxyPassScheme { span: SourceSpan, scheme: String },
    #[snafu(display("missing proxy_pass uri host"))]
    MissingProxyPassHost { span: SourceSpan },
    #[snafu(display("invalid socket address directive value"))]
    SocketAddr {
        span: SourceSpan,
        source: std::net::AddrParseError,
    },
    #[snafu(display("invalid port directive value"))]
    Port {
        span: SourceSpan,
        source: std::num::ParseIntError,
    },
    #[snafu(display("invalid header name directive value"))]
    HeaderName {
        span: SourceSpan,
        source: http::header::InvalidHeaderName,
    },
    #[snafu(display("invalid header value directive value"))]
    HeaderValue {
        span: SourceSpan,
        source: http::header::InvalidHeaderValue,
    },
    #[snafu(display("configured path does not exist"))]
    MissingPath { span: SourceSpan, path: PathBuf },
    #[snafu(display("invalid server name directive value"))]
    ServerName {
        span: SourceSpan,
        source: dhttp::name::InvalidDhttpName,
    },
    #[snafu(display("unsupported resolver kind `{kind}`"))]
    UnsupportedResolver { span: SourceSpan, kind: String },
    #[snafu(display("deprecated resolver kind `{kind}`"))]
    DeprecatedResolver { span: SourceSpan, kind: String },
    #[snafu(display("invalid ssh login method `{method}`"))]
    InvalidSshLogin { span: SourceSpan, method: String },
    #[snafu(display("missing ssh login method"))]
    MissingSshLogin { span: SourceSpan },
    #[snafu(display("invalid add_header always marker"))]
    InvalidAlways { span: SourceSpan, value: String },
    #[snafu(display("invalid stun_server bind directive"))]
    InvalidStunBind { span: SourceSpan },
}

pub fn parse_empty(
    _input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    Ok(ParsedDirective::Empty)
}

pub fn parse_string(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let arg = exactly_one(input.directive)?;
    Ok(ParsedDirective::Slot(TypedValue::new(
        StringConfig(arg.value.clone()),
        arg.span,
    )))
}

pub fn parse_string_list(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    Ok(ParsedDirective::Slot(TypedValue::new(
        StringList(
            input
                .directive
                .args
                .iter()
                .map(|arg| arg.value.clone())
                .collect(),
        ),
        input.directive.span,
    )))
}

pub fn parse_boolean(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let arg = exactly_one(input.directive)?;
    let value = match arg.value.as_str() {
        "on" => true,
        "off" => false,
        _ => {
            return Err(Box::new(ParseDirectiveValueError::InvalidBoolean {
                span: arg.span,
                value: arg.value.clone(),
            }));
        }
    };
    Ok(ParsedDirective::Slot(TypedValue::new(
        BoolConfig(value),
        arg.span,
    )))
}

pub fn parse_path(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let arg = exactly_one(input.directive)?;
    let path = PathBuf::from(&arg.value);
    Ok(ParsedDirective::Slot(TypedValue::new(
        PathConfig(path),
        arg.span,
    )))
}

pub fn parse_existing_path(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let arg = exactly_one(input.directive)?;
    let path = PathBuf::from(&arg.value);
    ensure!(
        path.exists(),
        parse_directive_value_error::MissingPathSnafu {
            span: arg.span,
            path
        }
    );
    Ok(ParsedDirective::Slot(TypedValue::new(
        PathConfig(path),
        arg.span,
    )))
}

pub fn parse_default_type(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let arg = exactly_one(input.directive)?;
    let value = HeaderValue::from_str(&arg.value)
        .context(parse_directive_value_error::HeaderValueSnafu { span: arg.span })?;
    Ok(ParsedDirective::Slot(TypedValue::new(
        DefaultType(value),
        arg.span,
    )))
}

pub fn parse_proxy_pass(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let arg = exactly_one(input.directive)?;
    let uri = arg
        .value
        .parse::<Uri>()
        .context(parse_directive_value_error::UriSnafu { span: arg.span })?;
    let scheme = uri
        .scheme_str()
        .context(parse_directive_value_error::MissingProxyPassSchemeSnafu { span: arg.span })?;
    ensure!(
        matches!(scheme, "http" | "https"),
        parse_directive_value_error::UnsupportedProxyPassSchemeSnafu {
            span: arg.span,
            scheme: scheme.to_owned()
        }
    );
    uri.host()
        .context(parse_directive_value_error::MissingProxyPassHostSnafu { span: arg.span })?;
    Ok(ParsedDirective::Slot(TypedValue::new(
        ProxyPass(uri),
        arg.span,
    )))
}

pub fn parse_resolver(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let args = &input.directive.args;
    if args.len() != 2 {
        return Err(Box::new(ParseDirectiveValueError::InvalidArgumentCount {
            span: input.directive.span,
            expected: "2",
            actual: args.len(),
        }));
    }
    match args[0].value.as_str() {
        "h3" => {
            let uri = args[1]
                .value
                .parse::<Uri>()
                .context(parse_directive_value_error::UriSnafu { span: args[1].span })?;
            Ok(ParsedDirective::Slot(TypedValue::new(
                ResolverConfig(uri),
                input.directive.span,
            )))
        }
        "udp" | "http" => Err(Box::new(ParseDirectiveValueError::DeprecatedResolver {
            span: args[0].span,
            kind: args[0].value.clone(),
        })),
        kind => Err(Box::new(ParseDirectiveValueError::UnsupportedResolver {
            span: args[0].span,
            kind: kind.to_owned(),
        })),
    }
}

pub fn parse_server_id(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let arg = exactly_one(input.directive)?;
    let id = arg
        .value
        .parse::<u8>()
        .context(parse_directive_value_error::PortSnafu { span: arg.span })?;
    Ok(ParsedDirective::Slot(TypedValue::new(
        ServerIdConfig(id),
        arg.span,
    )))
}

pub fn parse_server_name(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let mut names = Vec::with_capacity(input.directive.args.len());
    for arg in &input.directive.args {
        let name = dhttp::name::DhttpName::try_from(arg.value.clone())
            .context(parse_directive_value_error::ServerNameSnafu { span: arg.span })?;
        names.push(ServerName { name });
    }
    Ok(ParsedDirective::Slot(TypedValue::new(
        ServerNames(names),
        input.directive.span,
    )))
}

pub fn parse_listen(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let args = &input.directive.args;
    let listen = match args.as_slice() {
        [iface] => {
            if iface.value.contains(',') || iface.value.parse::<SocketAddr>().is_ok() {
                let addrs = parse_socket_addrs(&iface.value, iface.span)?;
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
            match IpFamilies::from_str(&param.value) {
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
                        .context(parse_directive_value_error::PortSnafu { span: param.span })?;
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
            let port = port
                .value
                .parse::<u16>()
                .context(parse_directive_value_error::PortSnafu { span: port.span })?;
            Listens {
                range: IfaceRange::from(iface.value.as_str()),
                families: IpFamilies::from_str(&version.value)?,
                port,
                specific_addrs: None,
            }
        }
        _ => {
            return Err(Box::new(ParseDirectiveValueError::InvalidArgumentCount {
                span: input.directive.span,
                expected: "1, 2, or 3",
                actual: args.len(),
            }));
        }
    };
    Ok(ParsedDirective::Slot(TypedValue::new(
        ListenConfig(vec![listen]),
        input.directive.span,
    )))
}

pub fn parse_address(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let arg = exactly_one(input.directive)?;
    let addrs = parse_socket_addrs(&arg.value, arg.span)?;
    Ok(ParsedDirective::Slot(TypedValue::new(addrs, arg.span)))
}

pub fn parse_stun_bind(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let args = &input.directive.args;
    if args.is_empty() {
        return Err(Box::new(ParseDirectiveValueError::InvalidArgumentCount {
            span: input.directive.span,
            expected: "at least 1",
            actual: args.len(),
        }));
    }
    let bind = args[0]
        .value
        .parse::<SocketAddr>()
        .context(parse_directive_value_error::SocketAddrSnafu { span: args[0].span })?;
    let mut value = StunBindConfigValue {
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
                    parse_directive_value_error::SocketAddrSnafu {
                        span: args[index + 1].span,
                    },
                )?);
                index += 2;
            }
            "change_addr" if index + 1 < args.len() => {
                value.change_addr = Some(args[index + 1].value.parse::<SocketAddr>().context(
                    parse_directive_value_error::SocketAddrSnafu {
                        span: args[index + 1].span,
                    },
                )?);
                index += 2;
            }
            "change_port" if index + 1 < args.len() => {
                value.change_port = Some(args[index + 1].value.parse::<u16>().context(
                    parse_directive_value_error::PortSnafu {
                        span: args[index + 1].span,
                    },
                )?);
                index += 2;
            }
            _ => {
                return Err(Box::new(ParseDirectiveValueError::InvalidStunBind {
                    span: args[index].span,
                }));
            }
        }
    }
    Ok(ParsedDirective::Slot(TypedValue::new(
        value,
        input.directive.span,
    )))
}

pub fn parse_header(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    parse_header_inner(input, false)
}

pub fn parse_header_always(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    parse_header_inner(input, true)
}

fn parse_header_inner(
    input: &DirectiveInput<'_>,
    allow_always: bool,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let args = &input.directive.args;
    let always = match args.as_slice() {
        [_, _] => !allow_always,
        [_, _, marker] if allow_always && marker.value == "always" => true,
        [_, _, marker] if allow_always => {
            return Err(Box::new(ParseDirectiveValueError::InvalidAlways {
                span: marker.span,
                value: marker.value.clone(),
            }));
        }
        _ => {
            return Err(Box::new(ParseDirectiveValueError::InvalidArgumentCount {
                span: input.directive.span,
                expected: if allow_always { "2 or 3" } else { "2" },
                actual: args.len(),
            }));
        }
    };
    let name = HeaderName::from_bytes(args[0].value.as_bytes())
        .context(parse_directive_value_error::HeaderNameSnafu { span: args[0].span })?;
    let value = HeaderValue::from_bytes(args[1].value.as_bytes())
        .context(parse_directive_value_error::HeaderValueSnafu { span: args[1].span })?;
    Ok(ParsedDirective::Slot(TypedValue::new(
        HeaderRules(vec![HeaderRule {
            name,
            value,
            always,
        }]),
        input.directive.span,
    )))
}

pub fn parse_types_raw_block(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let AstBody::Block { children, .. } = &input.directive.body else {
        return Err(Box::new(ParseDirectiveValueError::InvalidArgumentCount {
            span: input.directive.span,
            expected: "block",
            actual: 0,
        }));
    };
    let mut map = HashMap::new();
    for directive in children {
        let value = HeaderValue::from_str(&directive.name.value).context(
            parse_directive_value_error::HeaderValueSnafu {
                span: directive.name.span,
            },
        )?;
        for arg in &directive.args {
            map.insert(arg.value.clone(), value.clone());
        }
    }
    Ok(ParsedDirective::Slot(TypedValue::new(
        MimeTypes(map),
        input.directive.span,
    )))
}

pub fn parse_ssh_login(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    if input.directive.args.is_empty() {
        return Err(Box::new(ParseDirectiveValueError::MissingSshLogin {
            span: input.directive.span,
        }));
    }
    for arg in &input.directive.args {
        if arg.value != "ssl" {
            return Err(Box::new(ParseDirectiveValueError::InvalidSshLogin {
                span: arg.span,
                method: arg.value.clone(),
            }));
        }
    }
    parse_string_list(input)
}

pub fn parse_ssh_ssl_user(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let args = &input.directive.args;
    if args.len() != 2 {
        return Err(Box::new(ParseDirectiveValueError::InvalidArgumentCount {
            span: input.directive.span,
            expected: "2",
            actual: args.len(),
        }));
    }
    Ok(ParsedDirective::Slot(TypedValue::new(
        SshSslUsers(vec![SshSslUser {
            name: args[0].value.clone(),
            user: args[1].value.clone(),
        }]),
        input.directive.span,
    )))
}

pub fn parse_location_payload(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    let pattern = crate::parse::pattern::parse_spanned_pattern(&input.directive.args)?;
    Ok(ParsedDirective::Payload(TypedValue::new(
        pattern,
        input.directive.span,
    )))
}

fn exactly_one(directive: &AstDirective) -> Result<&Spanned<String>, ParseDirectiveValueError> {
    if directive.args.len() == 1 {
        Ok(&directive.args[0])
    } else {
        Err(ParseDirectiveValueError::InvalidArgumentCount {
            span: directive.span,
            expected: "1",
            actual: directive.args.len(),
        })
    }
}

fn parse_socket_addrs(
    value: &str,
    span: SourceSpan,
) -> Result<Vec<SocketAddr>, ParseDirectiveValueError> {
    value
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<SocketAddr>()
                .context(parse_directive_value_error::SocketAddrSnafu { span })
        })
        .collect()
}

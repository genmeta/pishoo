use std::{path::Path, sync::Arc};

use dhttp::home::{DhttpHome, identity::IdentityProfile};
use http::{HeaderName, HeaderValue};
use snafu::{ResultExt, Snafu};

use super::{
    ast::{AstBody, AstDirective},
    config::{
        AccessLogDirective, Cascaded, ConfigOrigin, EffectiveHttpConfig, LocationConfig,
        OriginScope, PishooConfig, PreparedProxyTlsPaths, RootWorkerDefaultsSnapshot, ServerConfig,
        ServerIdentity,
    },
    decode::{ConfigContext, DirectiveInput, DirectiveValue},
    domain::{ConfigDocumentIdAllocator, ConfigSourceSpan, ResolvedConfigPath},
    error::{ConfigLoadFailure, LoadConfigError},
    source::{ConfigDocumentSourceMap, SourceMap, SourceSpan},
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, HeaderRule,
        HeaderRules, MimeTypes, ProxyPass, ServerNames, StringList,
    },
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildTypedConfigError {
    #[snafu(display("unknown directive `{name}` in {context}"))]
    UnknownDirective {
        name: String,
        context: &'static str,
        span: SourceSpan,
    },
    #[snafu(display("directive `{name}` has the wrong shape"))]
    Shape { name: String, span: SourceSpan },
    #[snafu(display("duplicate directive `{name}`"))]
    Duplicate {
        name: String,
        first: SourceSpan,
        duplicate: SourceSpan,
    },
    #[snafu(display("failed to parse directive `{name}`"))]
    Directive {
        name: String,
        span: SourceSpan,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[snafu(display("missing required directive `{name}`"))]
    Missing {
        name: &'static str,
        span: SourceSpan,
    },
    #[snafu(display("configuration must contain exactly one pishoo block"))]
    PishooCardinality { span: SourceSpan },
    #[snafu(display("worker configuration cannot contain server blocks"))]
    WorkerServer { span: SourceSpan },
    #[snafu(display("server certificate and private key must be configured together"))]
    ServerTlsPair { span: SourceSpan },
    #[snafu(display("standalone server requires certificate and private key"))]
    StandaloneTls { span: SourceSpan },
    #[snafu(display("implicit profile TLS requires exactly one server_name"))]
    AmbiguousProfile { span: SourceSpan },
    #[snafu(display("identity server must bind exactly its profile name"))]
    IdentityName { span: SourceSpan },
    #[snafu(display("identity server cannot override profile TLS paths"))]
    IdentityTls { span: SourceSpan },
    #[snafu(display("proxy certificate and private key must be configured together"))]
    ProxyTlsPair { span: SourceSpan },
    #[snafu(display("proxy_pass cannot include a URI part in a regex location"))]
    RegexProxyUri { span: SourceSpan },
    #[snafu(display("access_log default is invalid for a direct server"))]
    DirectDefaultAccessLog { span: SourceSpan },
    #[snafu(display("access_log expects exactly one argument"))]
    AccessLogArgument { span: SourceSpan },
    #[snafu(display("forward proxy requires exactly one listen address"))]
    ForwardListen { span: SourceSpan },
}

#[derive(Debug)]
pub struct ServerConfigCandidate {
    source: ConfigSourceSpan,
    result: Result<ServerConfig, BuildTypedConfigError>,
}
impl ServerConfigCandidate {
    pub const fn source(&self) -> ConfigSourceSpan {
        self.source
    }
    pub fn result(&self) -> &Result<ServerConfig, BuildTypedConfigError> {
        &self.result
    }
    pub fn into_result(self) -> Result<ServerConfig, BuildTypedConfigError> {
        self.result
    }
}

#[derive(Debug)]
pub struct ParsedPishooConfig {
    pishoo: PishooConfig,
    servers: Box<[ServerConfigCandidate]>,
}
impl ParsedPishooConfig {
    pub fn pishoo(&self) -> &PishooConfig {
        &self.pishoo
    }
    pub fn servers(&self) -> &[ServerConfigCandidate] {
        &self.servers
    }
    pub fn into_parts(self) -> (PishooConfig, Box<[ServerConfigCandidate]>) {
        (self.pishoo, self.servers)
    }
}

#[derive(Debug)]
pub struct IdentityServerCandidate {
    profile: IdentityProfile,
    result: Result<ServerConfig, BuildTypedConfigError>,
}
impl IdentityServerCandidate {
    pub fn profile(&self) -> &IdentityProfile {
        &self.profile
    }
    pub fn result(&self) -> &Result<ServerConfig, BuildTypedConfigError> {
        &self.result
    }
    pub fn into_parts(self) -> (IdentityProfile, Result<ServerConfig, BuildTypedConfigError>) {
        (self.profile, self.result)
    }
}

#[derive(Default)]
struct LocalHttp {
    access_rules: Option<Assignment<AccessRulesUri>>,
    gzip: Option<Assignment<BoolConfig>>,
    gzip_vary: Option<Assignment<BoolConfig>>,
    gzip_min_length: Option<Assignment<GzipMinLength>>,
    gzip_comp_level: Option<Assignment<GzipCompLevel>>,
    gzip_types: Option<Assignment<StringList>>,
    default_type: Option<Assignment<DefaultType>>,
    types: Option<Assignment<MimeTypes>>,
    access_log: Option<Assignment<AccessLogDirective>>,
}

struct Assignment<T> {
    value: T,
    span: SourceSpan,
}

impl<T> Assignment<T> {
    fn new(value: T, span: SourceSpan) -> Self {
        Self { value, span }
    }

    fn map<U>(self, map: impl FnOnce(T) -> U) -> Assignment<U> {
        Assignment::new(map(self.value), self.span)
    }
}

pub struct TypedConfigParser {
    ids: ConfigDocumentIdAllocator,
}
impl Default for TypedConfigParser {
    fn default() -> Self {
        Self::new()
    }
}
impl TypedConfigParser {
    pub fn new() -> Self {
        Self {
            ids: ConfigDocumentIdAllocator::new(),
        }
    }

    pub fn parse_root(
        &mut self,
        text: &str,
        path: &Path,
        home: Option<&DhttpHome>,
    ) -> Result<ParsedPishooConfig, ConfigLoadFailure> {
        let (sources, directives) = self.syntax(text, path)?;
        self.build_pishoo(
            &sources,
            &directives,
            builtin_http(),
            OriginScope::RootPishoo,
            home,
            false,
        )
        .map_err(|source| failure(source, sources))
    }

    pub fn parse_worker(
        &mut self,
        text: &str,
        path: &Path,
        home: &DhttpHome,
        root: &RootWorkerDefaultsSnapshot,
    ) -> Result<ParsedPishooConfig, ConfigLoadFailure> {
        let (sources, directives) = self.syntax(text, path)?;
        self.build_pishoo(
            &sources,
            &directives,
            root.http().clone(),
            OriginScope::WorkerPishoo,
            Some(home),
            true,
        )
        .map_err(|source| failure(source, sources))
    }

    pub fn parse_identity(
        &mut self,
        text: &str,
        path: &Path,
        profile: IdentityProfile,
        defaults: &RootWorkerDefaultsSnapshot,
    ) -> Result<IdentityServerCandidate, ConfigLoadFailure> {
        let (sources, directives) = self.syntax(text, path)?;
        let source = directives
            .first()
            .map_or(SourceSpan::new(super::source::SourceId(0), 0, 0), |d| {
                d.span
            });
        if directives.len() != 1 || directives[0].name.value != "server" {
            return Err(failure(
                BuildTypedConfigError::Missing {
                    name: "server",
                    span: source,
                },
                sources,
            ));
        }
        let result = build_server(
            &directives[0],
            &sources,
            defaults.http(),
            Some(&profile),
            None,
        );
        Ok(IdentityServerCandidate { profile, result })
    }

    fn syntax(
        &mut self,
        text: &str,
        path: &Path,
    ) -> Result<(Arc<ConfigDocumentSourceMap>, Vec<AstDirective>), ConfigLoadFailure> {
        let document_id = self.ids.allocate().map_err(|source| ConfigLoadFailure {
            error: LoadConfigError::DocumentId { source },
            source_map: Arc::new(SourceMap::default()),
            document_id: None,
        })?;
        let mut source_map = SourceMap::default();
        let source_id = source_map.add_source(
            Some(path.to_path_buf()),
            Arc::from(text),
            path.parent().map(Path::to_path_buf),
            None,
        );
        let directives = match super::grammar::parse_source(text, source_id)
            .context(super::error::load_config_error::ParseFileSnafu { source_id })
        {
            Ok(directives) => directives,
            Err(error) => {
                return Err(ConfigLoadFailure {
                    error,
                    source_map: Arc::new(source_map),
                    document_id: Some(document_id),
                });
            }
        };
        let directives =
            match super::include::expand_includes(directives, &mut source_map, path.parent())
                .context(super::error::load_config_error::ResolveIncludeSnafu)
            {
                Ok(directives) => directives,
                Err(error) => {
                    return Err(ConfigLoadFailure {
                        error,
                        source_map: Arc::new(source_map),
                        document_id: Some(document_id),
                    });
                }
            };
        Ok((
            Arc::new(ConfigDocumentSourceMap::new(
                document_id,
                Arc::new(source_map),
            )),
            directives,
        ))
    }

    fn build_pishoo(
        &self,
        sources: &Arc<ConfigDocumentSourceMap>,
        directives: &[AstDirective],
        parent: EffectiveHttpConfig,
        scope: OriginScope,
        home: Option<&DhttpHome>,
        worker: bool,
    ) -> Result<ParsedPishooConfig, BuildTypedConfigError> {
        if directives.len() != 1 || directives[0].name.value != "pishoo" {
            return Err(BuildTypedConfigError::PishooCardinality {
                span: directives
                    .first()
                    .map_or(SourceSpan::new(super::source::SourceId(0), 0, 0), |d| {
                        d.span
                    }),
            });
        }
        let pishoo = &directives[0];
        let children = block(pishoo)?;
        let mut pid = None;
        let mut workers = None;
        let mut groups = None;
        let mut http = LocalHttp::default();
        let mut seen = std::collections::HashMap::new();
        for directive in children {
            if directive.name.value == "server" {
                if worker {
                    return Err(BuildTypedConfigError::WorkerServer {
                        span: directive.span,
                    });
                }
                continue;
            }
            if directive.name.value == "proxy" {
                if worker {
                    return Err(BuildTypedConfigError::UnknownDirective {
                        name: "proxy".to_owned(),
                        context: "worker pishoo",
                        span: directive.span,
                    });
                }
                continue;
            }
            reject_duplicate(&mut seen, directive)?;
            match directive.name.value.as_str() {
                "pid" => pid = Some(parse(directive, ConfigContext::Pishoo, sources)?),
                "workers" => workers = Some(parse(directive, ConfigContext::Pishoo, sources)?),
                "groups" => groups = Some(parse(directive, ConfigContext::Pishoo, sources)?),
                name if parse_http(name, directive, ConfigContext::Pishoo, sources, &mut http)? => {
                }
                name => {
                    return Err(BuildTypedConfigError::UnknownDirective {
                        name: name.to_owned(),
                        context: "pishoo",
                        span: directive.span,
                    });
                }
            }
        }
        let effective = overlay_http(&parent, http, scope, sources.source_map());
        // Server candidates are rebuilt after all PISHOO directives are known, so textual order is irrelevant.
        let forward_proxies = children
            .iter()
            .filter(|d| d.name.value == "proxy")
            .map(|directive| build_forward_proxy(directive, sources))
            .collect::<Result<Box<[_]>, _>>()?;
        let servers = children
            .iter()
            .filter(|d| d.name.value == "server")
            .map(|directive| {
                let source = sources.config_span(directive.span);
                ServerConfigCandidate {
                    source,
                    result: build_server(directive, sources, &effective, None, home),
                }
            })
            .collect();
        Ok(ParsedPishooConfig {
            pishoo: PishooConfig::new(
                sources.config_span(pishoo.span),
                pid,
                workers,
                groups,
                effective,
                forward_proxies,
            ),
            servers,
        })
    }
}

fn failure(
    source: BuildTypedConfigError,
    sources: Arc<ConfigDocumentSourceMap>,
) -> ConfigLoadFailure {
    ConfigLoadFailure {
        error: LoadConfigError::BuildTyped { source },
        source_map: Arc::clone(sources.source_map_arc()),
        document_id: Some(sources.document_id()),
    }
}

fn block(directive: &AstDirective) -> Result<&[AstDirective], BuildTypedConfigError> {
    match &directive.body {
        AstBody::Block { children, .. } => Ok(children),
        _ => Err(BuildTypedConfigError::Shape {
            name: directive.name.value.clone(),
            span: directive.span,
        }),
    }
}
fn leaf(directive: &AstDirective) -> Result<(), BuildTypedConfigError> {
    if directive.is_leaf() {
        Ok(())
    } else {
        Err(BuildTypedConfigError::Shape {
            name: directive.name.value.clone(),
            span: directive.span,
        })
    }
}
fn parse<T>(
    directive: &AstDirective,
    context: ConfigContext,
    sources: &ConfigDocumentSourceMap,
) -> Result<T, BuildTypedConfigError>
where
    T: DirectiveValue,
    for<'a, 'b> T: TryFrom<&'a DirectiveInput<'b>, Error = <T as DirectiveValue>::Error>,
{
    if !matches!(directive.name.value.as_str(), "types" | "stun_server") {
        leaf(directive)?;
    }
    T::try_from(&DirectiveInput {
        directive,
        context,
        source_map: sources.source_map(),
    })
    .map_err(|source| BuildTypedConfigError::Directive {
        name: directive.name.value.clone(),
        span: directive.span,
        source: Box::new(source),
    })
}
fn reject_duplicate(
    seen: &mut std::collections::HashMap<String, SourceSpan>,
    directive: &AstDirective,
) -> Result<(), BuildTypedConfigError> {
    if matches!(
        directive.name.value.as_str(),
        "listen" | "location" | "stun_server" | "add_header" | "proxy_set_header" | "ssh_ssl_user"
    ) {
        return Ok(());
    }
    if let Some(first) = seen.insert(directive.name.value.clone(), directive.span) {
        return Err(BuildTypedConfigError::Duplicate {
            name: directive.name.value.clone(),
            first,
            duplicate: directive.span,
        });
    }
    Ok(())
}

fn builtin<T>(value: T) -> Cascaded<T> {
    Cascaded::new(value, vec![ConfigOrigin::builtin()].into_boxed_slice())
}
fn builtin_http() -> EffectiveHttpConfig {
    EffectiveHttpConfig::new(
        builtin(None),
        builtin(BoolConfig(false)),
        builtin(BoolConfig(false)),
        builtin(GzipMinLength(20)),
        builtin(GzipCompLevel(1)),
        builtin(StringList(Vec::new())),
        builtin(None),
        builtin(None),
        builtin(AccessLogDirective::Off),
    )
}
fn cascade<T: Clone>(
    parent: &Cascaded<T>,
    local: Option<Assignment<T>>,
    scope: OriginScope,
    sources: &SourceMap,
) -> Cascaded<T> {
    match local {
        Some(assignment) => {
            let mut lineage = parent.lineage().to_vec();
            lineage.push(ConfigOrigin::source(scope, sources, assignment.span));
            Cascaded::new(assignment.value, lineage.into_boxed_slice())
        }
        None => parent.clone(),
    }
}
fn overlay_http(
    parent: &EffectiveHttpConfig,
    local: LocalHttp,
    scope: OriginScope,
    sources: &SourceMap,
) -> EffectiveHttpConfig {
    EffectiveHttpConfig::new(
        cascade(
            parent.access_rules(),
            local.access_rules.map(|value| value.map(Some)),
            scope,
            sources,
        ),
        cascade(parent.gzip(), local.gzip, scope, sources),
        cascade(parent.gzip_vary(), local.gzip_vary, scope, sources),
        cascade(
            parent.gzip_min_length(),
            local.gzip_min_length,
            scope,
            sources,
        ),
        cascade(
            parent.gzip_comp_level(),
            local.gzip_comp_level,
            scope,
            sources,
        ),
        cascade(parent.gzip_types(), local.gzip_types, scope, sources),
        cascade(
            parent.default_type(),
            local.default_type.map(|value| value.map(Some)),
            scope,
            sources,
        ),
        cascade(
            parent.types(),
            local.types.map(|value| value.map(Some)),
            scope,
            sources,
        ),
        cascade(parent.access_log(), local.access_log, scope, sources),
    )
}
fn parse_http(
    name: &str,
    d: &AstDirective,
    c: ConfigContext,
    s: &ConfigDocumentSourceMap,
    h: &mut LocalHttp,
) -> Result<bool, BuildTypedConfigError> {
    match name {
        "access_rules" => h.access_rules = Some(parse_assignment(d, c, s)?),
        "gzip" => h.gzip = Some(parse_assignment(d, c, s)?),
        "gzip_vary" => h.gzip_vary = Some(parse_assignment(d, c, s)?),
        "gzip_min_length" => h.gzip_min_length = Some(parse_assignment(d, c, s)?),
        "gzip_comp_level" => h.gzip_comp_level = Some(parse_assignment(d, c, s)?),
        "gzip_types" => h.gzip_types = Some(parse_assignment(d, c, s)?),
        "default_type" => h.default_type = Some(parse_assignment(d, c, s)?),
        "types" => h.types = Some(parse_assignment(d, c, s)?),
        "access_log" => {
            h.access_log = Some(Assignment::new(parse_access_log(d, c, s)?, d.span));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_assignment<T>(
    directive: &AstDirective,
    context: ConfigContext,
    sources: &ConfigDocumentSourceMap,
) -> Result<Assignment<T>, BuildTypedConfigError>
where
    T: DirectiveValue,
    for<'a, 'b> T: TryFrom<&'a DirectiveInput<'b>, Error = <T as DirectiveValue>::Error>,
{
    Ok(Assignment::new(
        parse(directive, context, sources)?,
        directive.span,
    ))
}
fn parse_access_log(
    d: &AstDirective,
    c: ConfigContext,
    s: &ConfigDocumentSourceMap,
) -> Result<AccessLogDirective, BuildTypedConfigError> {
    leaf(d)?;
    if d.args.len() != 1 {
        return Err(BuildTypedConfigError::AccessLogArgument { span: d.span });
    }
    Ok(match d.args[0].value.as_str() {
        "off" => AccessLogDirective::Off,
        "default" => AccessLogDirective::ProfileDefault,
        _ => AccessLogDirective::Resolved(parse::<ResolvedConfigPath>(d, c, s)?),
    })
}

fn build_forward_proxy(
    d: &AstDirective,
    s: &ConfigDocumentSourceMap,
) -> Result<crate::forward::ForwardConfig, BuildTypedConfigError> {
    let children = block(d)?;
    let mut seen = std::collections::HashMap::new();
    let mut listen = None;
    let mut allow = Vec::new();
    let mut deny = Vec::new();
    for directive in children {
        reject_duplicate(&mut seen, directive)?;
        match directive.name.value.as_str() {
            "listen" => {
                let addresses: super::types::SocketAddrs =
                    parse(directive, ConfigContext::Pishoo, s)?;
                if addresses.0.len() != 1 {
                    return Err(BuildTypedConfigError::ForwardListen {
                        span: directive.span,
                    });
                }
                listen = addresses.0.into_iter().next();
            }
            "allow" => allow = parse::<StringList>(directive, ConfigContext::Pishoo, s)?.0,
            "deny" => deny = parse::<StringList>(directive, ConfigContext::Pishoo, s)?.0,
            name => {
                return Err(BuildTypedConfigError::UnknownDirective {
                    name: name.to_owned(),
                    context: "proxy",
                    span: directive.span,
                });
            }
        }
    }
    let Some(listen) = listen else {
        return Err(BuildTypedConfigError::ForwardListen { span: d.span });
    };
    Ok(crate::forward::ForwardConfig {
        listen,
        allow,
        deny,
    })
}

fn build_server(
    d: &AstDirective,
    s: &ConfigDocumentSourceMap,
    parent: &EffectiveHttpConfig,
    profile: Option<&IdentityProfile>,
    home: Option<&DhttpHome>,
) -> Result<ServerConfig, BuildTypedConfigError> {
    let children = block(d)?;
    let mut seen = std::collections::HashMap::new();
    let mut listens = Vec::new();
    let mut names = None;
    let mut resolver = None;
    let mut cert = None;
    let mut key = None;
    let mut relay = None;
    let mut stun = None;
    let mut stun_servers = Vec::new();
    let mut http = LocalHttp::default();
    let mut locations = Vec::new();
    for x in children {
        reject_duplicate(&mut seen, x)?;
        match x.name.value.as_str() {
            "listen" => listens.push(parse(x, ConfigContext::Server, s)?),
            "server_name" => names = Some(parse::<ServerNames>(x, ConfigContext::Server, s)?),
            "dns" => resolver = Some(parse(x, ConfigContext::Server, s)?),
            "ssl_certificate" => cert = Some(parse(x, ConfigContext::Server, s)?),
            "ssl_certificate_key" => key = Some(parse(x, ConfigContext::Server, s)?),
            "relay" => relay = Some(parse(x, ConfigContext::Server, s)?),
            "stun" => stun = Some(parse(x, ConfigContext::Server, s)?),
            "stun_server" => stun_servers.push(parse(x, ConfigContext::Server, s)?),
            "location" => locations.push(x),
            name if parse_http(name, x, ConfigContext::Server, s, &mut http)? => {}
            name => {
                return Err(BuildTypedConfigError::UnknownDirective {
                    name: name.to_owned(),
                    context: "server",
                    span: x.span,
                });
            }
        }
    }
    if listens.is_empty() {
        return Err(BuildTypedConfigError::Missing {
            name: "listen",
            span: d.span,
        });
    }
    let names = names
        .map(|v| v.0.into_iter().map(|v| v.name).collect::<Box<[_]>>())
        .unwrap_or_else(|| {
            profile
                .map(|p| vec![p.name().clone()].into_boxed_slice())
                .unwrap_or_default()
        });
    let identity = if let Some(p) = profile {
        if names.as_ref() != [p.name().clone()] {
            return Err(BuildTypedConfigError::IdentityName { span: d.span });
        }
        if cert.is_some() || key.is_some() {
            return Err(BuildTypedConfigError::IdentityTls { span: d.span });
        }
        ServerIdentity::Profile(p.clone())
    } else {
        match (cert, key) {
            (Some(c), Some(k)) => ServerIdentity::Direct {
                certificate: c,
                private_key: k,
            },
            (None, None) => {
                let Some(home) = home else {
                    return Err(BuildTypedConfigError::StandaloneTls { span: d.span });
                };
                if names.len() != 1 {
                    return Err(BuildTypedConfigError::AmbiguousProfile { span: d.span });
                }
                ServerIdentity::Profile(home.identity_profile(names[0].clone()))
            }
            _ => return Err(BuildTypedConfigError::ServerTlsPair { span: d.span }),
        }
    };
    let parent = if matches!(identity, ServerIdentity::Profile(_))
        && parent.access_log().lineage().len() == 1
    {
        parent
            .clone()
            .with_access_log(builtin(AccessLogDirective::ProfileDefault))
    } else {
        parent.clone()
    };
    let effective = overlay_http(&parent, http, OriginScope::Server, s.source_map());
    if matches!(identity, ServerIdentity::Direct { .. })
        && matches!(
            effective.access_log().effective(),
            AccessLogDirective::ProfileDefault
        )
    {
        return Err(BuildTypedConfigError::DirectDefaultAccessLog { span: d.span });
    }
    let locations = locations
        .into_iter()
        .map(|x| build_location(x, s, &effective))
        .collect::<Result<Box<[_]>, _>>()?;
    Ok(ServerConfig::new(
        identity,
        s.config_span(d.span),
        names,
        listens.into_boxed_slice(),
        resolver,
        effective,
        relay,
        stun,
        stun_servers.into_boxed_slice(),
        locations,
    ))
}

fn build_location(
    d: &AstDirective,
    s: &ConfigDocumentSourceMap,
    parent: &EffectiveHttpConfig,
) -> Result<LocationConfig, BuildTypedConfigError> {
    let matcher = super::pattern::parse_spanned_pattern(&d.args).map_err(|source| {
        BuildTypedConfigError::Directive {
            name: d.name.value.clone(),
            span: d.span,
            source: Box::new(source),
        }
    })?;
    let children = block(d)?;
    let mut seen = std::collections::HashMap::new();
    let mut root = None;
    let mut alias = None;
    let mut index = None;
    let mut add = Vec::new();
    let mut set = Vec::new();
    let mut pass = None;
    let mut cert = None;
    let mut key = None;
    let mut trusted = None;
    let mut login = None;
    let mut users = Vec::new();
    let mut deny = None;
    let mut http = LocalHttp::default();
    for x in children {
        reject_duplicate(&mut seen, x)?;
        match x.name.value.as_str() {
            "root" => root = Some(parse(x, ConfigContext::Location, s)?),
            "alias" => alias = Some(parse(x, ConfigContext::Location, s)?),
            "index" => index = Some(parse(x, ConfigContext::Location, s)?),
            "add_header" => add.push(parse(x, ConfigContext::Location, s)?),
            "proxy_set_header" => set.push(parse(x, ConfigContext::Location, s)?),
            "proxy_pass" => pass = Some(parse(x, ConfigContext::Location, s)?),
            "proxy_ssl_certificate" => cert = Some(parse(x, ConfigContext::Location, s)?),
            "proxy_ssl_certificate_key" => key = Some(parse(x, ConfigContext::Location, s)?),
            "proxy_ssl_trusted_certificate" => {
                trusted = Some(parse(x, ConfigContext::Location, s)?)
            }
            "ssh_login" => login = Some(parse(x, ConfigContext::Location, s)?),
            "ssh_ssl_user" => users.push(parse(x, ConfigContext::Location, s)?),
            "ssh_deny" => deny = Some(parse(x, ConfigContext::Location, s)?),
            name if parse_http(name, x, ConfigContext::Location, s, &mut http)? => {}
            name => {
                return Err(BuildTypedConfigError::UnknownDirective {
                    name: name.to_owned(),
                    context: "location",
                    span: x.span,
                });
            }
        }
    }
    let proxy_tls = match (cert, key) {
        (Some(c), Some(k)) => Some(PreparedProxyTlsPaths::new(Some(c), Some(k), trusted)),
        (None, None) => {
            trusted.map(|trusted| PreparedProxyTlsPaths::new(None, None, Some(trusted)))
        }
        _ => return Err(BuildTypedConfigError::ProxyTlsPair { span: d.span }),
    };
    if matches!(
        matcher,
        super::pattern::Pattern::Regex(_) | super::pattern::Pattern::CRegex(_)
    ) && pass.as_ref().is_some_and(ProxyPass::has_explicit_uri)
    {
        return Err(BuildTypedConfigError::RegexProxyUri { span: d.span });
    }
    add.push(HeaderRules(vec![HeaderRule {
        name: HeaderName::from_static("server"),
        value: HeaderValue::from_static("pishoo"),
        always: true,
    }]));
    let effective = overlay_http(parent, http, OriginScope::Location, s.source_map());
    Ok(LocationConfig::new(
        s.config_span(d.span),
        matcher,
        effective,
        root,
        alias,
        index,
        add.into_boxed_slice(),
        set.into_boxed_slice(),
        pass,
        proxy_tls,
        login,
        users.into_boxed_slice(),
        deny,
    ))
}

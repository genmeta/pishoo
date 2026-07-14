use std::path::{Path, PathBuf};

use dhttp::{home::identity::IdentityProfile, name::DhttpName};
use snafu::Snafu;

use super::{
    domain::{ConfigSourceSpan, ResolvedConfigPath},
    pattern::Pattern,
    source::{SourceMap, SourceSpan},
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, HeaderRules,
        ListenConfig, MimeTypes, ProxyPass, ResolverConfig, SshLoginMethods, SshSslUsers,
        StringList, StunServerConfigValue,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OriginScope {
    Builtin,
    RootPishoo,
    WorkerPishoo,
    Server,
    Location,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConfigOrigin {
    scope: OriginScope,
    path: Option<PathBuf>,
    line: Option<u32>,
    column: Option<u32>,
}

impl ConfigOrigin {
    pub(crate) fn builtin() -> Self {
        Self::new(OriginScope::Builtin, None)
    }
    pub(crate) fn source(scope: OriginScope, sources: &SourceMap, span: SourceSpan) -> Self {
        let location = sources.line_column(span);
        Self {
            scope,
            path: sources.path_for_span(span).map(Path::to_path_buf),
            line: location.and_then(|location| u32::try_from(location.line).ok()),
            column: location.and_then(|location| u32::try_from(location.column).ok()),
        }
    }
    fn new(scope: OriginScope, path: Option<PathBuf>) -> Self {
        Self {
            scope,
            path,
            line: None,
            column: None,
        }
    }
    pub const fn scope(&self) -> OriginScope {
        self.scope
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub const fn line(&self) -> Option<u32> {
        self.line
    }
    pub const fn column(&self) -> Option<u32> {
        self.column
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cascaded<T> {
    effective: T,
    lineage: Box<[ConfigOrigin]>,
}

impl<T> Cascaded<T> {
    pub(crate) fn new(effective: T, lineage: Box<[ConfigOrigin]>) -> Self {
        Self { effective, lineage }
    }
    pub fn effective(&self) -> &T {
        &self.effective
    }
    pub fn lineage(&self) -> &[ConfigOrigin] {
        &self.lineage
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccessLogDirective {
    Off,
    ProfileDefault,
    Resolved(ResolvedConfigPath),
}

impl AccessLogDirective {
    pub fn resolved_path(&self) -> Option<&Path> {
        match self {
            Self::Resolved(path) => Some(path.as_ref()),
            _ => None,
        }
    }

    pub fn materialize(
        &self,
        identity: &ServerIdentity,
    ) -> Result<ResolvedAccessLogConfig, MaterializeAccessLogError> {
        match (self, identity) {
            (Self::Off, _) => Ok(ResolvedAccessLogConfig::Disabled),
            (Self::Resolved(path), _) => Ok(ResolvedAccessLogConfig::Enabled(path.clone())),
            (Self::ProfileDefault, ServerIdentity::Profile(profile)) => {
                let path = ResolvedConfigPath::try_from(profile.access_log_path())
                    .map_err(|source| MaterializeAccessLogError::InvalidProfilePath { source })?;
                Ok(ResolvedAccessLogConfig::Enabled(path))
            }
            (Self::ProfileDefault, ServerIdentity::Direct { .. }) => {
                Err(MaterializeAccessLogError::DirectProfileDefault)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedAccessLogConfig {
    Disabled,
    Enabled(ResolvedConfigPath),
}

#[derive(Debug, Snafu)]
pub enum MaterializeAccessLogError {
    #[snafu(display("direct server cannot use the identity-profile access log default"))]
    DirectProfileDefault,
    #[snafu(display("identity-profile access log path is invalid"))]
    InvalidProfilePath {
        source: super::domain::ResolvedConfigPathError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveHttpConfig {
    access_rules: Cascaded<Option<AccessRulesUri>>,
    gzip: Cascaded<BoolConfig>,
    gzip_vary: Cascaded<BoolConfig>,
    gzip_min_length: Cascaded<GzipMinLength>,
    gzip_comp_level: Cascaded<GzipCompLevel>,
    gzip_types: Cascaded<StringList>,
    default_type: Cascaded<Option<DefaultType>>,
    types: Cascaded<Option<MimeTypes>>,
    access_log: Cascaded<AccessLogDirective>,
}

impl EffectiveHttpConfig {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        access_rules: Cascaded<Option<AccessRulesUri>>,
        gzip: Cascaded<BoolConfig>,
        gzip_vary: Cascaded<BoolConfig>,
        gzip_min_length: Cascaded<GzipMinLength>,
        gzip_comp_level: Cascaded<GzipCompLevel>,
        gzip_types: Cascaded<StringList>,
        default_type: Cascaded<Option<DefaultType>>,
        types: Cascaded<Option<MimeTypes>>,
        access_log: Cascaded<AccessLogDirective>,
    ) -> Self {
        Self {
            access_rules,
            gzip,
            gzip_vary,
            gzip_min_length,
            gzip_comp_level,
            gzip_types,
            default_type,
            types,
            access_log,
        }
    }
    pub fn access_rules(&self) -> &Cascaded<Option<AccessRulesUri>> {
        &self.access_rules
    }
    pub fn gzip(&self) -> &Cascaded<BoolConfig> {
        &self.gzip
    }
    pub fn gzip_vary(&self) -> &Cascaded<BoolConfig> {
        &self.gzip_vary
    }
    pub fn gzip_min_length(&self) -> &Cascaded<GzipMinLength> {
        &self.gzip_min_length
    }
    pub fn gzip_comp_level(&self) -> &Cascaded<GzipCompLevel> {
        &self.gzip_comp_level
    }
    pub fn gzip_types(&self) -> &Cascaded<StringList> {
        &self.gzip_types
    }
    pub fn default_type(&self) -> &Cascaded<Option<DefaultType>> {
        &self.default_type
    }
    pub fn types(&self) -> &Cascaded<Option<MimeTypes>> {
        &self.types
    }
    pub fn access_log(&self) -> &Cascaded<AccessLogDirective> {
        &self.access_log
    }
    pub(crate) fn with_access_log(mut self, access_log: Cascaded<AccessLogDirective>) -> Self {
        self.access_log = access_log;
        self
    }
}

#[derive(Clone, Debug)]
pub struct PishooConfig {
    source: ConfigSourceSpan,
    pid: Option<ResolvedConfigPath>,
    workers: Option<StringList>,
    groups: Option<StringList>,
    http: EffectiveHttpConfig,
    forward_proxies: Box<[crate::forward::ForwardConfig]>,
}

impl PishooConfig {
    pub(crate) fn new(
        source: ConfigSourceSpan,
        pid: Option<ResolvedConfigPath>,
        workers: Option<StringList>,
        groups: Option<StringList>,
        http: EffectiveHttpConfig,
        forward_proxies: Box<[crate::forward::ForwardConfig]>,
    ) -> Self {
        Self {
            source,
            pid,
            workers,
            groups,
            http,
            forward_proxies,
        }
    }
    pub const fn source(&self) -> ConfigSourceSpan {
        self.source
    }
    pub fn pid(&self) -> Option<&ResolvedConfigPath> {
        self.pid.as_ref()
    }
    pub fn workers(&self) -> Option<&StringList> {
        self.workers.as_ref()
    }
    pub fn groups(&self) -> Option<&StringList> {
        self.groups.as_ref()
    }
    pub fn http(&self) -> &EffectiveHttpConfig {
        &self.http
    }
    pub fn forward_proxies(&self) -> &[crate::forward::ForwardConfig] {
        &self.forward_proxies
    }
    pub fn worker_defaults(&self) -> RootWorkerDefaultsSnapshot {
        RootWorkerDefaultsSnapshot::from_http(self.http.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootWorkerDefaultsSnapshot {
    http: EffectiveHttpConfig,
}
impl RootWorkerDefaultsSnapshot {
    pub(crate) fn from_http(http: EffectiveHttpConfig) -> Self {
        Self { http }
    }
    pub fn http(&self) -> &EffectiveHttpConfig {
        &self.http
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CascadedWire<T> {
    effective: T,
    lineage: Box<[ConfigOrigin]>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct RootWorkerDefaultsWire {
    access_rules: CascadedWire<Option<String>>,
    gzip: CascadedWire<bool>,
    gzip_vary: CascadedWire<bool>,
    gzip_min_length: CascadedWire<u64>,
    gzip_comp_level: CascadedWire<i32>,
    gzip_types: CascadedWire<Vec<String>>,
    default_type: CascadedWire<Option<Vec<u8>>>,
    types: CascadedWire<Option<MimeTypesWire>>,
    access_log: CascadedWire<AccessLogWire>,
}
type MimeTypesWire = Vec<(String, Vec<u8>)>;
#[derive(serde::Serialize, serde::Deserialize)]
enum AccessLogWire {
    Off,
    ProfileDefault,
    Resolved(PathBuf),
}
fn encode<T, W>(value: &Cascaded<T>, f: impl FnOnce(&T) -> W) -> CascadedWire<W> {
    CascadedWire {
        effective: f(value.effective()),
        lineage: value.lineage().into(),
    }
}
fn decode<T, W>(
    value: CascadedWire<W>,
    f: impl FnOnce(W) -> Result<T, String>,
) -> Result<Cascaded<T>, String> {
    Ok(Cascaded::new(f(value.effective)?, value.lineage))
}
impl serde::Serialize for RootWorkerDefaultsSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let h = &self.http;
        RootWorkerDefaultsWire {
            access_rules: encode(h.access_rules(), |v| {
                v.as_ref().map(|v| v.0.as_str().to_owned())
            }),
            gzip: encode(h.gzip(), |v| v.0),
            gzip_vary: encode(h.gzip_vary(), |v| v.0),
            gzip_min_length: encode(h.gzip_min_length(), |v| v.0),
            gzip_comp_level: encode(h.gzip_comp_level(), |v| v.0),
            gzip_types: encode(h.gzip_types(), |v| v.0.clone()),
            default_type: encode(h.default_type(), |v| {
                v.as_ref().map(|v| v.0.as_bytes().to_vec())
            }),
            types: encode(h.types(), |v| {
                v.as_ref().map(|v| {
                    v.0.iter()
                        .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
                        .collect()
                })
            }),
            access_log: encode(h.access_log(), |v| match v {
                AccessLogDirective::Off => AccessLogWire::Off,
                AccessLogDirective::ProfileDefault => AccessLogWire::ProfileDefault,
                AccessLogDirective::Resolved(p) => {
                    AccessLogWire::Resolved(p.as_ref().to_path_buf())
                }
            }),
        }
        .serialize(serializer)
    }
}
impl<'de> serde::Deserialize<'de> for RootWorkerDefaultsSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let w = RootWorkerDefaultsWire::deserialize(deserializer)?;
        let access_rules = decode(w.access_rules, |v| {
            v.map(|v| {
                url::Url::parse(&v)
                    .map_err(|e| e.to_string())
                    .and_then(|u| AccessRulesUri::try_from(u).map_err(|e| e.to_string()))
            })
            .transpose()
        })
        .map_err(D::Error::custom)?;
        let gzip = decode(w.gzip, |v| Ok(BoolConfig(v))).map_err(D::Error::custom)?;
        let gzip_vary = decode(w.gzip_vary, |v| Ok(BoolConfig(v))).map_err(D::Error::custom)?;
        let gzip_min_length = decode(w.gzip_min_length, |v| Ok(GzipMinLength::checked(v)))
            .map_err(D::Error::custom)?;
        let gzip_comp_level = decode(w.gzip_comp_level, |v| Ok(GzipCompLevel::checked(v)))
            .map_err(D::Error::custom)?;
        let gzip_types = decode(w.gzip_types, |v| {
            StringList::checked_gzip_types(v).map_err(|e| e.to_string())
        })
        .map_err(D::Error::custom)?;
        let default_type = decode(w.default_type, |v| {
            v.map(|v| DefaultType::checked_from_bytes(&v).map_err(|e| e.to_string()))
                .transpose()
        })
        .map_err(D::Error::custom)?;
        let types = decode(w.types, |v| {
            v.map(|v| MimeTypes::checked_from_bytes(v).map_err(|e| e.to_string()))
                .transpose()
        })
        .map_err(D::Error::custom)?;
        let access_log = decode(w.access_log, |v| {
            Ok(match v {
                AccessLogWire::Off => AccessLogDirective::Off,
                AccessLogWire::ProfileDefault => AccessLogDirective::ProfileDefault,
                AccessLogWire::Resolved(p) => AccessLogDirective::Resolved(
                    ResolvedConfigPath::try_from(p).map_err(|e| e.to_string())?,
                ),
            })
        })
        .map_err(D::Error::custom)?;
        Ok(Self::from_http(EffectiveHttpConfig::new(
            access_rules,
            gzip,
            gzip_vary,
            gzip_min_length,
            gzip_comp_level,
            gzip_types,
            default_type,
            types,
            access_log,
        )))
    }
}

#[derive(Clone, Debug)]
pub enum ServerIdentity {
    Direct {
        certificate: ResolvedConfigPath,
        private_key: ResolvedConfigPath,
    },
    Profile(IdentityProfile),
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    identity: ServerIdentity,
    source: ConfigSourceSpan,
    names: Box<[DhttpName<'static>]>,
    listens: Box<[ListenConfig]>,
    resolver: Option<ResolverConfig>,
    http: EffectiveHttpConfig,
    relay: Option<BoolConfig>,
    stun: Option<BoolConfig>,
    stun_servers: Box<[StunServerConfigValue]>,
    locations: Box<[LocationConfig]>,
}

impl ServerConfig {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        identity: ServerIdentity,
        source: ConfigSourceSpan,
        names: Box<[DhttpName<'static>]>,
        listens: Box<[ListenConfig]>,
        resolver: Option<ResolverConfig>,
        http: EffectiveHttpConfig,
        relay: Option<BoolConfig>,
        stun: Option<BoolConfig>,
        stun_servers: Box<[StunServerConfigValue]>,
        locations: Box<[LocationConfig]>,
    ) -> Self {
        Self {
            identity,
            source,
            names,
            listens,
            resolver,
            http,
            relay,
            stun,
            stun_servers,
            locations,
        }
    }
    pub fn identity(&self) -> &ServerIdentity {
        &self.identity
    }
    pub const fn source(&self) -> ConfigSourceSpan {
        self.source
    }
    pub fn names(&self) -> &[DhttpName<'static>] {
        &self.names
    }
    pub fn listens(&self) -> &[ListenConfig] {
        &self.listens
    }
    pub fn resolver(&self) -> Option<&ResolverConfig> {
        self.resolver.as_ref()
    }
    pub fn http(&self) -> &EffectiveHttpConfig {
        &self.http
    }
    pub fn relay(&self) -> Option<&BoolConfig> {
        self.relay.as_ref()
    }
    pub fn stun(&self) -> Option<&BoolConfig> {
        self.stun.as_ref()
    }
    pub fn stun_servers(&self) -> &[StunServerConfigValue] {
        &self.stun_servers
    }
    pub fn locations(&self) -> &[LocationConfig] {
        &self.locations
    }
}

#[derive(Clone, Debug)]
pub struct PreparedProxyTlsPaths {
    certificate: Option<ResolvedConfigPath>,
    private_key: Option<ResolvedConfigPath>,
    trusted_certificate: Option<ResolvedConfigPath>,
}
impl PreparedProxyTlsPaths {
    pub(crate) fn new(
        certificate: Option<ResolvedConfigPath>,
        private_key: Option<ResolvedConfigPath>,
        trusted_certificate: Option<ResolvedConfigPath>,
    ) -> Self {
        Self {
            certificate,
            private_key,
            trusted_certificate,
        }
    }
    pub fn certificate(&self) -> Option<&ResolvedConfigPath> {
        self.certificate.as_ref()
    }
    pub fn private_key(&self) -> Option<&ResolvedConfigPath> {
        self.private_key.as_ref()
    }
    pub fn trusted_certificate(&self) -> Option<&ResolvedConfigPath> {
        self.trusted_certificate.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct LocationConfig {
    source: ConfigSourceSpan,
    matcher: Pattern,
    http: EffectiveHttpConfig,
    root: Option<ResolvedConfigPath>,
    alias: Option<ResolvedConfigPath>,
    index: Option<StringList>,
    add_headers: Box<[HeaderRules]>,
    proxy_set_headers: Box<[HeaderRules]>,
    proxy_pass: Option<ProxyPass>,
    proxy_tls: Option<PreparedProxyTlsPaths>,
    ssh_login: Option<SshLoginMethods>,
    ssh_users: Box<[SshSslUsers]>,
    ssh_deny: Option<StringList>,
}
impl LocationConfig {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        source: ConfigSourceSpan,
        matcher: Pattern,
        http: EffectiveHttpConfig,
        root: Option<ResolvedConfigPath>,
        alias: Option<ResolvedConfigPath>,
        index: Option<StringList>,
        add_headers: Box<[HeaderRules]>,
        proxy_set_headers: Box<[HeaderRules]>,
        proxy_pass: Option<ProxyPass>,
        proxy_tls: Option<PreparedProxyTlsPaths>,
        ssh_login: Option<SshLoginMethods>,
        ssh_users: Box<[SshSslUsers]>,
        ssh_deny: Option<StringList>,
    ) -> Self {
        Self {
            source,
            matcher,
            http,
            root,
            alias,
            index,
            add_headers,
            proxy_set_headers,
            proxy_pass,
            proxy_tls,
            ssh_login,
            ssh_users,
            ssh_deny,
        }
    }
    pub const fn source(&self) -> ConfigSourceSpan {
        self.source
    }
    pub fn matcher(&self) -> &Pattern {
        &self.matcher
    }
    pub fn http(&self) -> &EffectiveHttpConfig {
        &self.http
    }
    pub fn root(&self) -> Option<&ResolvedConfigPath> {
        self.root.as_ref()
    }
    pub fn alias(&self) -> Option<&ResolvedConfigPath> {
        self.alias.as_ref()
    }
    pub fn index(&self) -> Option<&StringList> {
        self.index.as_ref()
    }
    pub fn add_headers(&self) -> &[HeaderRules] {
        &self.add_headers
    }
    pub fn proxy_set_headers(&self) -> &[HeaderRules] {
        &self.proxy_set_headers
    }
    pub fn proxy_pass(&self) -> Option<&ProxyPass> {
        self.proxy_pass.as_ref()
    }
    pub fn proxy_tls(&self) -> Option<&PreparedProxyTlsPaths> {
        self.proxy_tls.as_ref()
    }
    pub fn ssh_login(&self) -> Option<&SshLoginMethods> {
        self.ssh_login.as_ref()
    }
    pub fn ssh_users(&self) -> &[SshSslUsers] {
        &self.ssh_users
    }
    pub fn ssh_deny(&self) -> Option<&StringList> {
        self.ssh_deny.as_ref()
    }
}

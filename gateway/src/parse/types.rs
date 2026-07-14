use std::{fmt, net::SocketAddr, path::PathBuf, str::FromStr};

use dhttp::{h3x::dquic::binds::BindPattern, name::DhttpName};
use snafu::{Snafu, whatever};

use super::Result;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerName {
    pub name: DhttpName<'static>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoolConfig(pub bool);

#[derive(Debug, Clone)]
pub struct StringConfig(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringList(pub Vec<String>);

#[derive(Debug, Clone)]
pub struct PathConfig(pub std::path::PathBuf);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRulesUri(pub url::Url);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyPass {
    pub raw: String,
    pub uri: http::Uri,
    pub proxy_host: String,
    pub explicit_path_and_query: Option<String>,
}

impl ProxyPass {
    pub fn has_explicit_uri(&self) -> bool {
        self.explicit_path_and_query.is_some()
    }

    pub fn explicit_path_and_query(&self) -> Option<&str> {
        self.explicit_path_and_query.as_deref()
    }

    pub fn scheme_str(&self) -> &str {
        self.uri
            .scheme_str()
            .expect("proxy_pass uri is validated during parsing")
    }

    pub fn host(&self) -> &str {
        self.uri
            .host()
            .expect("proxy_pass uri is validated during parsing")
    }

    pub fn port_u16(&self) -> Option<u16> {
        self.uri.port_u16()
    }
}

#[derive(Debug, Clone)]
pub struct ResolverConfig(pub http::Uri);

#[derive(Debug, Clone)]
pub struct SocketAddrs(pub Vec<std::net::SocketAddr>);

#[derive(Debug, Clone)]
pub struct ListenConfig(pub Vec<Listens>);

#[derive(Debug, Clone)]
pub struct ServerNames(pub Vec<ServerName>);

#[derive(Debug, Clone)]
pub struct ClientNameConfig(pub DhttpName<'static>);

#[derive(Debug, Clone)]
pub struct HeaderRule {
    pub name: http::HeaderName,
    pub value: http::HeaderValue,
    pub always: bool,
}

#[derive(Debug, Clone)]
pub struct HeaderRules(pub Vec<HeaderRule>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimeTypes(pub std::collections::HashMap<String, http::HeaderValue>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultType(pub http::HeaderValue);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GzipMinLength(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GzipCompLevel(pub i32);

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum AccessRulesUriValidationError {
    #[snafu(display("unsupported access_rules uri scheme `{scheme}`"))]
    UnsupportedScheme { scheme: String },
    #[snafu(display("unsupported sqlite access_rules uri form"))]
    UnsupportedSqliteForm,
    #[snafu(display("access_rules sqlite path must be absolute"))]
    RelativePath,
    #[snafu(display("access_rules sqlite path contains a NUL byte"))]
    NulPath,
    #[snafu(display("access_rules sqlite path is not valid UTF-8 on this platform"))]
    InvalidPathEncoding,
}

impl TryFrom<url::Url> for AccessRulesUri {
    type Error = AccessRulesUriValidationError;

    fn try_from(uri: url::Url) -> Result<Self, Self::Error> {
        let path = Self::decoded_sqlite_path(&uri)?;
        if !path.is_absolute() {
            return Err(AccessRulesUriValidationError::RelativePath);
        }
        Ok(Self(uri))
    }
}

impl AccessRulesUri {
    pub(crate) fn decoded_sqlite_path(
        uri: &url::Url,
    ) -> Result<PathBuf, AccessRulesUriValidationError> {
        if uri.scheme() != "sqlite" {
            return Err(AccessRulesUriValidationError::UnsupportedScheme {
                scheme: uri.scheme().to_owned(),
            });
        }
        if uri.host_str().is_some()
            || !uri.username().is_empty()
            || uri.password().is_some()
            || uri.port().is_some()
            || uri.fragment().is_some()
        {
            return Err(AccessRulesUriValidationError::UnsupportedSqliteForm);
        }
        let path = percent_encoding::percent_decode_str(uri.path()).collect::<Vec<_>>();
        if path.contains(&0) {
            return Err(AccessRulesUriValidationError::NulPath);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            Ok(std::ffi::OsString::from_vec(path).into())
        }
        #[cfg(not(unix))]
        {
            String::from_utf8(path)
                .map(PathBuf::from)
                .map_err(|_| AccessRulesUriValidationError::InvalidPathEncoding)
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum GzipTypesValidationError {
    #[snafu(display("gzip_types contains an empty MIME token"))]
    EmptyToken,
}

impl StringList {
    pub fn checked_gzip_types(values: Vec<String>) -> Result<Self, GzipTypesValidationError> {
        if values.iter().any(String::is_empty) {
            return Err(GzipTypesValidationError::EmptyToken);
        }
        Ok(Self(values))
    }
}

impl GzipMinLength {
    pub const fn checked(value: u64) -> Self {
        Self(value)
    }
}

impl GzipCompLevel {
    pub const fn checked(value: i32) -> Self {
        Self(value)
    }
}

impl DefaultType {
    pub fn checked_from_bytes(value: &[u8]) -> Result<Self, http::header::InvalidHeaderValue> {
        http::HeaderValue::from_bytes(value).map(Self)
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum MimeTypesValidationError {
    #[snafu(display("MIME extension must not be empty"))]
    EmptyExtension,
    #[snafu(display("duplicate MIME extension `{extension}`"))]
    DuplicateExtension { extension: String },
    #[snafu(display("invalid MIME type header value"))]
    HeaderValue {
        source: http::header::InvalidHeaderValue,
    },
}

impl MimeTypes {
    pub fn checked_from_bytes<I>(entries: I) -> Result<Self, MimeTypesValidationError>
    where
        I: IntoIterator<Item = (String, Vec<u8>)>,
    {
        let mut values = std::collections::HashMap::new();
        for (extension, value) in entries {
            if extension.is_empty() {
                return Err(MimeTypesValidationError::EmptyExtension);
            }
            let value = http::HeaderValue::from_bytes(&value)
                .map_err(|source| MimeTypesValidationError::HeaderValue { source })?;
            if values.insert(extension.clone(), value).is_some() {
                return Err(MimeTypesValidationError::DuplicateExtension { extension });
            }
        }
        Ok(Self(values))
    }
}

#[derive(Debug, Clone)]
pub struct SshLoginMethods(pub Vec<String>);

#[derive(Debug, Clone)]
pub struct SshSslUser {
    pub name: String,
    pub user: String,
}

#[derive(Debug, Clone)]
pub struct SshSslUsers(pub Vec<SshSslUser>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StunBindConfigValue {
    pub bind: std::net::SocketAddr,
    pub outer_addr: Option<std::net::SocketAddr>,
    pub change_addr: Option<std::net::SocketAddr>,
    pub change_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StunChangePort(pub u16);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StunServerConfigValue {
    pub binds: Box<[StunBindConfigValue]>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IpFamilies {
    V4,
    V6,
    #[default]
    Dual,
}

impl FromStr for IpFamilies {
    type Err = crate::error::Whatever;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "v4only" => Ok(IpFamilies::V4),
            "v6only" => Ok(IpFamilies::V6),
            "dual" => Ok(IpFamilies::Dual),
            _ => whatever!("invalid ip families: {s}, expected `v4only`, `v6only` or `dual`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IfaceRange {
    All,
    External,
    Internal,
    Exact(String),
}

impl IfaceRange {
    pub fn contains(&self, iface_name: &str) -> bool {
        match self {
            IfaceRange::All => true,
            IfaceRange::Exact(name) => name == iface_name,
            IfaceRange::Internal => matches!(iface_name, "lo" | "lo0"),
            IfaceRange::External => {
                tracing::warn!(
                    "iface range external is not implemented yet, treating as non-match"
                );
                false
            }
        }
    }
}

impl fmt::Display for IfaceRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::External => f.write_str("external"),
            Self::Internal => f.write_str("internal"),
            Self::Exact(name) => f.write_str(name),
        }
    }
}

impl From<&str> for IfaceRange {
    fn from(value: &str) -> Self {
        match value {
            "all" => IfaceRange::All,
            "external" => IfaceRange::External,
            "internal" => IfaceRange::Internal,
            _ => IfaceRange::Exact(value.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Listens {
    pub range: IfaceRange,
    pub families: IpFamilies,
    pub port: u16,
    pub specific_addrs: Option<Vec<SocketAddr>>,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ListenBindPatternError {
    #[snafu(display("unsupported listen iface range `{range}`"))]
    UnsupportedIfaceRange { range: IfaceRange },
    #[snafu(display("failed to construct generated bind pattern `{input}`"))]
    GeneratedBindPattern {
        input: String,
        source: peg::error::ParseError<peg::str::LineCol>,
    },
}

impl Listens {
    pub fn new(range: IfaceRange, families: IpFamilies, port: u16) -> Self {
        Self {
            range,
            families,
            port,
            specific_addrs: None,
        }
    }

    pub fn try_to_bind_patterns(&self) -> Result<Vec<BindPattern>, ListenBindPatternError> {
        fn parse_pattern(input: String) -> Result<BindPattern, ListenBindPatternError> {
            input
                .parse()
                .map_err(|source| ListenBindPatternError::GeneratedBindPattern { input, source })
        }

        if let Some(specific_addrs) = &self.specific_addrs {
            return specific_addrs
                .iter()
                .map(|addr| parse_pattern(format!("inet://{addr}")))
                .collect();
        }

        let host = match &self.range {
            IfaceRange::All => "*",
            IfaceRange::Exact(name) => name.as_str(),
            IfaceRange::Internal => {
                return Ok(match self.families {
                    IpFamilies::V4 => {
                        vec![parse_pattern(format!("inet://127.0.0.1:{}", self.port))?]
                    }
                    IpFamilies::V6 => {
                        vec![parse_pattern(format!("inet://[::1]:{}", self.port))?]
                    }
                    IpFamilies::Dual => vec![
                        parse_pattern(format!("inet://127.0.0.1:{}", self.port))?,
                        parse_pattern(format!("inet://[::1]:{}", self.port))?,
                    ],
                });
            }
            IfaceRange::External => {
                return listen_bind_pattern_error::UnsupportedIfaceRangeSnafu {
                    range: self.range.clone(),
                }
                .fail();
            }
        };

        let family_prefix = match self.families {
            IpFamilies::V4 => "v4.",
            IpFamilies::V6 => "v6.",
            IpFamilies::Dual => "",
        };

        Ok(vec![parse_pattern(format!(
            "iface://{family_prefix}{host}:{}",
            self.port
        ))?])
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    use super::*;

    fn pattern_strings(listen: Listens) -> Vec<String> {
        listen
            .try_to_bind_patterns()
            .expect("listen should produce bind patterns")
            .into_iter()
            .map(|pattern| pattern.to_string())
            .collect()
    }

    fn try_pattern_strings(listen: Listens) -> Result<Vec<String>, ListenBindPatternError> {
        listen.try_to_bind_patterns().map(|patterns| {
            patterns
                .into_iter()
                .map(|pattern| pattern.to_string())
                .collect()
        })
    }

    #[test]
    fn listens_all_dual_preserves_wildcard_pattern() {
        let listen = Listens::new(IfaceRange::All, IpFamilies::Dual, 443);

        assert_eq!(pattern_strings(listen), vec!["iface://*:443"]);
    }

    #[test]
    fn listens_exact_family_preserves_family_pattern() {
        assert_eq!(
            pattern_strings(Listens::new(
                IfaceRange::Exact("eth0".to_owned()),
                IpFamilies::V4,
                443
            )),
            vec!["iface://v4.eth0:443"]
        );
        assert_eq!(
            pattern_strings(Listens::new(
                IfaceRange::Exact("eth0".to_owned()),
                IpFamilies::V6,
                443
            )),
            vec!["iface://v6.eth0:443"]
        );
    }

    #[test]
    fn listens_specific_addrs_become_inet_patterns() {
        let mut listen = Listens::new(IfaceRange::All, IpFamilies::Dual, 443);
        listen.specific_addrs = Some(vec![
            SocketAddr::from((Ipv4Addr::LOCALHOST, 8443)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, 9443)),
        ]);

        assert_eq!(
            pattern_strings(listen),
            vec!["inet://127.0.0.1:8443", "inet://[::1]:9443"]
        );
    }

    #[test]
    fn listens_internal_dual_becomes_loopback_patterns() {
        assert_eq!(
            try_pattern_strings(Listens::new(IfaceRange::Internal, IpFamilies::Dual, 443))
                .expect("internal dual listen should be supported"),
            vec!["inet://127.0.0.1:443", "inet://[::1]:443"]
        );
    }

    #[test]
    fn listens_internal_family_becomes_matching_loopback_pattern() {
        assert_eq!(
            try_pattern_strings(Listens::new(IfaceRange::Internal, IpFamilies::V4, 443))
                .expect("internal v4 listen should be supported"),
            vec!["inet://127.0.0.1:443"]
        );
        assert_eq!(
            try_pattern_strings(Listens::new(IfaceRange::Internal, IpFamilies::V6, 443))
                .expect("internal v6 listen should be supported"),
            vec!["inet://[::1]:443"]
        );
    }

    #[test]
    fn listens_external_returns_typed_error() {
        let error = Listens::new(IfaceRange::External, IpFamilies::Dual, 443)
            .try_to_bind_patterns()
            .expect_err("external listen should be explicitly unsupported");

        assert!(matches!(
            error,
            ListenBindPatternError::UnsupportedIfaceRange { .. }
        ));
    }
}

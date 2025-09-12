use std::net::SocketAddr;

use thiserror::Error;

pub type Result<T, E = CustomError> = std::result::Result<T, E>;

/// TODO: refine error types
#[derive(Error, Debug)]
pub enum CustomError {
    #[error("invalid directive")]
    InvalidDirective(String),
    #[error("invalid config")]
    ConfigError(String),
    #[error("unknown directive")]
    UnknownDirective(String),
    #[error("invalid address")]
    AddrParseError(#[from] std::net::AddrParseError),
    #[error("missing config")]
    MissingConfig(String),
    #[error("file not found")]
    FileNotFound(String),
    #[error("missing arg")]
    MissingArg(String),
    #[error("unsupported config")]
    UnsupportedConfig(String),
    #[error("io error")]
    IoError(#[from] std::io::Error),
    #[error("h3 stream error")]
    H3Error(#[from] h3::error::StreamError),
    #[error("http error")]
    HttpError(#[from] http::Error),
    #[error("regex error")]
    RegexError(#[from] regex::Error),
    #[error("header conversion error")]
    ToStrError(#[from] http::header::ToStrError),
    #[error("uri parse error")]
    InvalidUri(#[from] http::uri::InvalidUri),
    #[error("hyper error")]
    HyperError(#[from] hyper::Error),
    #[error("missing host")]
    MissingHost,
    #[error("router not found")]
    RouterNotFound(String),
    #[error("duplicate server addr: `{0:?}`")]
    DuplicateServer(SocketAddr),
    #[error("invalid arg")]
    InvalidArgs(String),
    #[error("invalid config")]
    InvalidConfig(String),
    #[error("missing field")]
    MissingField(String),
    #[error("Localhost not initialized")]
    LocalhostNotInitialized,
    #[error(transparent)]
    Whatever(#[from] Whatever),
    #[error("unknown data store error")]
    Unknown,
}

#[derive(Debug, snafu::Snafu)]
#[snafu(whatever)]
#[snafu(display("{message}"))]
#[snafu(provide(opt, ref, chain, dyn snafu::Error + Send + Sync => source.as_deref()))]
pub struct Whatever {
    #[snafu(source(from(Box<dyn snafu::Error + Send + Sync>, Some)))]
    #[snafu(provide(false))]
    source: Option<Box<dyn snafu::Error + Send + Sync>>,
    message: String,
    backtrace: snafu::Backtrace,
}

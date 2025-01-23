use std::net::SocketAddr;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, CustomError>;

#[derive(Error, Debug)]
pub enum CustomError {
    #[error("unknown directive: `{0}`")]
    UnknownDirective(String),
    #[error("invalid address: `{0}`")]
    AddrParseError(#[from] std::net::AddrParseError),
    #[error("missing config: `{0}`")]
    MissingConfig(String),
    #[error("file not found: `{0}`")]
    FileNotFound(String),
    #[error("missing arg: `{0}`")]
    MissingArg(String),
    #[error("unsupported config: `{0}`")]
    UnsupportedConfig(String),
    #[error("io error: `{0}`")]
    IoError(#[from] std::io::Error),
    #[error("h3 error: `{0}`")]
    H3Error(#[from] h3::Error),
    #[error("http error: `{0}`")]
    HttpError(#[from] http::Error),
    #[error("regex error: `{0}`")]
    RegexError(#[from] regex::Error),
    #[error("http error: `{0}`")]
    ToStrError(#[from] http::header::ToStrError),
    #[error("uri parse error: `{0}`")]
    InvalidUri(#[from] http::uri::InvalidUri),
    #[error("hyper error: `{0}`")]
    HyperError(#[from] hyper::Error),
    #[error("missing host")]
    MissingHost,
    #[error("router not found: `{0}`")]
    RouterNotFound(String),
    #[error("duplicate server addr: `{0:?}`")]
    DuplicateServer(SocketAddr),
    #[error("invalid arg: `{0}`")]
    InvalidArgs(String),
    #[error("invalid config: `{0}`")]
    InvalidConfig(String),
    #[error("missing field: `{0}`")]
    MissingField(String),
    #[error("unknown data store error")]
    Unknown,
}

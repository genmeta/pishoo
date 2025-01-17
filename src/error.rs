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
    #[error("missing arg: `{0}`")]
    MissingArg(String),
    #[error("unsupported config: `{0}`")]
    UnsupportedConfig(String),
    #[error("io error: `{0}`")]
    IoError(#[from] std::io::Error),
    #[error("h3 error: `{0}`")]
    H3Error(#[from] h3::Error),
    #[error("reqwest error: `{0}`")]
    ReqwestError(#[from] reqwest::Error),
    #[error("http error: `{0}`")]
    HttpError(#[from] http::Error),
    #[error("regex error: `{0}`")]
    RegexError(#[from] regex::Error),
    #[error("http error: `{0}`")]
    ToStrError(#[from] http::header::ToStrError),
    #[error("unknown data store error")]
    Unknown,
}

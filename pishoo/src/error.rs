use http::StatusCode;

#[derive(Debug)]
pub enum Error {
    BadRequest(String),
    InvalidConfig(String),
    InvalidComponent(String),
    InvalidIdentity(String),
    IdentityMismatch,
    MissingHandshake,
    RouteNotFound,
    MethodNotAllowed,
    Denied,
    Deadline,
    GuestExitedWithoutResponse,
    GuestRejectedResponse(wasmtime_wasi_http::p2::bindings::http::types::ErrorCode),
    Guest(wasmtime::Error),
    Io(std::io::Error),
    Database(sea_orm::DbErr),
    ConfigDatabase(rusqlite::Error),
    Http(http::Error),
    Dhttp(dhttp::Error),
    Task(tokio::task::JoinError),
    ShutdownDeadline,
}

impl Error {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::IdentityMismatch => StatusCode::MISDIRECTED_REQUEST,
            Self::RouteNotFound => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::Denied | Self::InvalidIdentity(_) => StatusCode::FORBIDDEN,
            Self::Deadline => StatusCode::GATEWAY_TIMEOUT,
            Self::Dhttp(_) => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    pub(crate) fn body_error(self) -> dhttp::BoxError {
        Box::new(self)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(s)
            | Self::InvalidConfig(s)
            | Self::InvalidComponent(s)
            | Self::InvalidIdentity(s) => f.write_str(s),
            Self::Guest(e) => write!(f, "guest execution: {e}"),
            Self::Io(e) => write!(f, "I/O: {e}"),
            Self::Database(e) => write!(f, "access database: {e}"),
            Self::ConfigDatabase(e) => write!(f, "configuration database: {e}"),
            Self::Http(e) => write!(f, "HTTP: {e}"),
            Self::Dhttp(e) => write!(f, "DHTTP: {e}"),
            Self::Task(e) => write!(f, "application task: {e}"),
            Self::GuestRejectedResponse(e) => write!(f, "guest rejected response: {e:?}"),
            Self::IdentityMismatch => f.write_str("request authority does not match endpoint"),
            Self::MissingHandshake => f.write_str("trusted handshake is missing"),
            Self::RouteNotFound => f.write_str("route not found"),
            Self::MethodNotAllowed => f.write_str("method not allowed"),
            Self::Denied => f.write_str("permission denied"),
            Self::Deadline => f.write_str("execution deadline exceeded"),
            Self::GuestExitedWithoutResponse => f.write_str("guest exited without a response"),
            Self::ShutdownDeadline => f.write_str("application shutdown deadline exceeded"),
        }
    }
}
impl std::error::Error for Error {}

macro_rules! from_error {
    ($source:ty, $variant:ident) => {
        impl From<$source> for Error {
            fn from(error: $source) -> Self {
                Self::$variant(error)
            }
        }
    };
}
from_error!(std::io::Error, Io);
from_error!(rusqlite::Error, ConfigDatabase);
from_error!(sea_orm::DbErr, Database);
from_error!(http::Error, Http);
from_error!(dhttp::Error, Dhttp);
from_error!(wasmtime::Error, Guest);
from_error!(tokio::task::JoinError, Task);

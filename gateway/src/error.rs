use snafu::Snafu;

pub type Result<T, E = CustomError> = std::result::Result<T, E>;

type AnyError = dyn std::error::Error + Send + Sync + 'static;
type BoxError = Box<AnyError>;

/// TODO: refine error types
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum CustomError {
    #[snafu(display("Invalid configuration"))]
    InvalidConfig {
        #[snafu(source(from(BoxError, std::convert::identity)))]
        source: BoxError,
    },

    #[snafu(display("Http3 stream I/O error"))]
    Stream { source: h3::error::StreamError },

    #[snafu(transparent)]
    Whatever { source: Whatever },

    #[snafu(display("Unknown error"))]
    Unknown,
}

#[derive(Debug, Snafu)]
#[snafu(whatever)]
#[snafu(display("{message}"))]
#[snafu(provide(opt, ref, chain, AnyError => source.as_deref()))]
pub struct Whatever {
    #[snafu(source(from(BoxError, Some)))]
    #[snafu(provide(false))]
    source: Option<BoxError>,
    message: String,
    backtrace: snafu::Backtrace,
}

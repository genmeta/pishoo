use snafu::Snafu;

pub type Result<T, E = CustomError> = std::result::Result<T, E>;

pub type AnyError = dyn std::error::Error + Send + Sync + 'static;
pub type BoxError = Box<AnyError>;

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

impl snafu::FromString for CustomError {
    type Source = <Whatever as snafu::FromString>::Source;

    fn without_source(message: String) -> Self {
        Whatever::without_source(message).into()
    }

    fn with_source(source: Self::Source, message: String) -> Self {
        Whatever::with_source(source, message).into()
    }
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

#[doc(hidden)]
#[macro_export]
macro_rules! invalid_config {
    ($fmt:literal$(, $($arg:expr),* $(,)?)?) => {
        return $crate::error::invalid_config!(
            ::core::result::Result::Err(
                <$crate::error::Whatever as ::snafu::FromString>::without_source(
                    format!($fmt$(, $($arg),*)*),
                )
            )
        )
    };
    ($source:expr) => {
        ::snafu::ResultExt::context(
            ::core::result::Result::map_err($source, $crate::error::BoxError::from),
            $crate::error::InvalidConfigSnafu {}
        )
    };
    ($source:expr, $fmt:literal$(, $($arg:expr),* $(,)?)*) => {
        $crate::error::invalid_config!(
            ::core::result::Result::map_err($source, |__source| {
                <$crate::error::Whatever as ::snafu::FromString>::with_source(
                    $crate::error::BoxError::from(__source),
                    format!($fmt$(, $($arg),*)*)
                )
            }),
        )
    };
}

pub use crate::invalid_config;

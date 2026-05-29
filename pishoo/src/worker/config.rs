//! Worker-side configuration source loading.
//!
//! Workers scan the configured DHTTP home for identities and return
//! [`WorkerServerSource`](crate::service::source::WorkerServerSource) values.
//! Runtime preparation happens in [`crate::service::source`].

use dhttp_home::DhttpHome;
use futures::StreamExt;
use gateway::error::Whatever;
use snafu::Snafu;

use crate::policy;

/// Errors during worker configuration loading.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildConfigError {
    #[snafu(transparent)]
    Whatever { source: Whatever },
    #[snafu(display("failed to load access rules"))]
    Policy { source: policy::PolicyError },
}

impl snafu::FromString for BuildConfigError {
    type Source = <Whatever as snafu::FromString>::Source;

    fn without_source(message: String) -> Self {
        Whatever::without_source(message).into()
    }

    fn with_source(source: Self::Source, message: String) -> Self {
        Whatever::with_source(source, message).into()
    }
}

pub async fn load_worker_server_sources(
    dhttp_home: &DhttpHome,
) -> Result<Vec<crate::service::source::WorkerServerSource>, BuildConfigError> {
    let mut identity_names = Vec::new();
    let mut stream = std::pin::pin!(dhttp_home.identity_profile_names());
    while let Some(result) = stream.next().await {
        match result {
            Ok(name) => identity_names.push(name),
            Err(error) => {
                tracing::warn!(
                    error = %snafu::Report::from_error(&error),
                    "failed to read identity entry, skipping"
                );
            }
        }
    }

    let mut sources = Vec::new();
    for name in identity_names {
        let identity_profile = match dhttp_home.resolve_identity_profile(name.borrow()).await {
            Ok(home) => home,
            Err(error) => {
                tracing::warn!(
                    %name,
                    error = %snafu::Report::from_error(&error),
                    "failed to load identity home, skipping"
                );
                continue;
            }
        };

        sources.push(crate::service::source::WorkerServerSource {
            name,
            identity_profile,
        });
    }

    Ok(sources)
}

//! Worker-side configuration source loading.
//!
//! Workers scan the configured DHTTP home for identities and return
//! [`IdentityServiceSource`](crate::service::source::IdentityServiceSource) values.
//! Runtime preparation happens in [`crate::service::source`].

use dhttp::home::DhttpHome;
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

pub async fn load_identity_service_sources(
    dhttp_home: &DhttpHome,
) -> Result<Vec<crate::service::source::IdentityServiceSource>, BuildConfigError> {
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

        let server_conf_path = identity_profile.server_conf_path();
        if !server_conf_path.is_file() {
            tracing::debug!(
                %name,
                path = %server_conf_path.display(),
                "identity profile has no server.conf, skipping"
            );
            continue;
        }

        sources.push(crate::service::source::IdentityServiceSource {
            name,
            home: dhttp_home.clone(),
            identity_profile,
        });
    }

    Ok(sources)
}

#[cfg(test)]
mod tests {
    use dhttp::{home::DhttpHome, name::DhttpName};

    use super::load_identity_service_sources;

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("pishoo-worker-config-{label}-{nanos}"))
    }

    #[tokio::test]
    async fn identity_service_sources_skip_profiles_without_server_conf() {
        let home = DhttpHome::new(unique_test_dir("server-conf-filter"));
        let with_conf = DhttpName::try_from("with-conf.dhttp.net".to_owned()).unwrap();
        let without_conf = DhttpName::try_from("without-conf.dhttp.net".to_owned()).unwrap();
        let with_conf_profile = home.identity_profile(with_conf.clone());
        let without_conf_profile = home.identity_profile(without_conf);

        tokio::fs::create_dir_all(with_conf_profile.ssl_dir())
            .await
            .expect("create profile with server.conf");
        tokio::fs::write(
            with_conf_profile.server_conf_path(),
            "server { listen all 443; }",
        )
        .await
        .expect("write server.conf");
        tokio::fs::create_dir_all(without_conf_profile.ssl_dir())
            .await
            .expect("create profile without server.conf");

        let sources = load_identity_service_sources(&home)
            .await
            .expect("load identity service sources");

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, with_conf);
    }
}

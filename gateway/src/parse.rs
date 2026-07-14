use std::path::Path;

use dhttp::home::{DhttpHome, identity::IdentityProfile};

pub mod ast;
pub mod build;
pub mod builtin;
pub mod config;
pub mod decode;
pub mod diagnostic;
pub mod domain;
pub mod error;
pub mod grammar;
pub mod include;
pub mod normalize;
pub mod pattern;
pub mod source;
pub mod types;

#[cfg(test)]
pub(crate) mod tests;

pub use build::{
    BuildTypedConfigError, IdentityServerCandidate, ParsedPishooConfig, ServerConfigCandidate,
    TypedConfigParser,
};

pub(crate) type Result<T, E = crate::error::Whatever> = std::result::Result<T, E>;

pub async fn load_root_config_file(
    parser: &mut TypedConfigParser,
    path: &Path,
    home: Option<&DhttpHome>,
) -> Result<ParsedPishooConfig, error::ConfigLoadFailure> {
    let text = read_config(path).await?;
    parser.parse_root(&text, path, home)
}

pub async fn load_worker_config_file(
    parser: &mut TypedConfigParser,
    path: &Path,
    home: &DhttpHome,
    root: &config::RootWorkerDefaultsSnapshot,
) -> Result<Option<ParsedPishooConfig>, error::ConfigLoadFailure> {
    let Some(text) = read_optional_config(path).await? else {
        return Ok(None);
    };
    parser.parse_worker(&text, path, home, root).map(Some)
}

pub async fn load_identity_config_file(
    parser: &mut TypedConfigParser,
    profile: IdentityProfile,
    root: &config::RootWorkerDefaultsSnapshot,
) -> Result<Option<IdentityServerCandidate>, error::ConfigLoadFailure> {
    let path = profile.server_conf_path();
    let Some(text) = read_optional_config(&path).await? else {
        return Ok(None);
    };
    parser.parse_identity(&text, &path, profile, root).map(Some)
}

async fn read_config(path: &Path) -> Result<String, error::ConfigLoadFailure> {
    tokio::fs::read_to_string(path)
        .await
        .map_err(|source| error::ConfigLoadFailure::read(path, source))
}

async fn read_optional_config(path: &Path) -> Result<Option<String>, error::ConfigLoadFailure> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(error::ConfigLoadFailure::read(path, source)),
    }
}

#[cfg(test)]
pub(crate) struct TestConfigFailure {
    pub(crate) error: TestConfigError,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TestConfigError(Box<dyn std::error::Error + Send + Sync>);

#[cfg(test)]
impl std::fmt::Display for TestConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
impl std::error::Error for TestConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[cfg(test)]
pub(crate) fn parse_config_str_for_test(text: &str) -> Result<(), TestConfigFailure> {
    let parsed = TypedConfigParser::new()
        .parse_root(text, Path::new("/tmp/pishoo.conf"), None)
        .map_err(|error| TestConfigFailure {
            error: TestConfigError(Box::new(error)),
        })?;
    let (_, candidates) = parsed.into_parts();
    for candidate in candidates {
        candidate.into_result().map_err(|error| TestConfigFailure {
            error: TestConfigError(Box::new(error)),
        })?;
    }
    Ok(())
}

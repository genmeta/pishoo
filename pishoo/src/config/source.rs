use std::path::{Path, PathBuf};

use dhttp::home::{DhttpHome, HomeScope};
use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ResolveConfigSourceError {
    #[snafu(display("failed to load global dhttp home"))]
    GlobalHome { source: dhttp::home::LoadDhttpHomeError },
}

#[derive(Debug, Clone)]
pub enum PishooConfigSource {
    GlobalHome {
        home: DhttpHome,
        config_path: PathBuf,
    },
    ExplicitFile {
        config_path: PathBuf,
    },
}

impl PishooConfigSource {
    pub const CONFIG_FILE_NAME: &'static str = "pishoo.conf";

    pub fn resolve(config_file: Option<PathBuf>) -> Result<Self, ResolveConfigSourceError> {
        match config_file {
            Some(config_path) => Ok(Self::explicit(config_path)),
            None => {
                let home = DhttpHome::load(HomeScope::Global)
                    .context(resolve_config_source_error::GlobalHomeSnafu)?;
                Ok(Self::from_global_home(home))
            }
        }
    }

    pub fn from_global_home(home: DhttpHome) -> Self {
        let config_path = home.join(Self::CONFIG_FILE_NAME);
        Self::GlobalHome { home, config_path }
    }

    pub fn explicit(config_path: PathBuf) -> Self {
        Self::ExplicitFile { config_path }
    }

    pub fn config_path(&self) -> &Path {
        match self {
            Self::GlobalHome { config_path, .. } | Self::ExplicitFile { config_path } => {
                config_path
            }
        }
    }

    pub fn dhttp_home(&self) -> Option<&DhttpHome> {
        match self {
            Self::GlobalHome { home, .. } => Some(home),
            Self::ExplicitFile { .. } => None,
        }
    }

    pub fn default_worker_groups_enabled(&self) -> bool {
        matches!(self, Self::GlobalHome { .. })
    }

    pub fn load_identity_services(&self) -> bool {
        matches!(self, Self::GlobalHome { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(path: &str) -> DhttpHome {
        DhttpHome::new(PathBuf::from(path))
    }

    #[test]
    fn global_home_source_uses_home_pishoo_conf() {
        let source = PishooConfigSource::from_global_home(home("/tmp/dhttp-global"));

        assert_eq!(
            source.config_path(),
            Path::new("/tmp/dhttp-global/pishoo.conf")
        );
        assert!(source.dhttp_home().is_some());
        assert!(source.default_worker_groups_enabled());
        assert!(source.load_identity_services());
    }

    #[test]
    fn explicit_file_source_has_no_home_context() {
        let source = PishooConfigSource::explicit(PathBuf::from("/tmp/custom.conf"));

        assert_eq!(source.config_path(), Path::new("/tmp/custom.conf"));
        assert!(source.dhttp_home().is_none());
        assert!(!source.default_worker_groups_enabled());
        assert!(!source.load_identity_services());
    }
}

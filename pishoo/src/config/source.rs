use std::path::{Path, PathBuf};

use dhttp::home::{DhttpHome, HomeScope};
use gateway::parse::domain::{ResolvedConfigPath, ResolvedConfigPathError};
use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ResolveConfigSourceError {
    #[snafu(display("failed to load global dhttp home"))]
    GlobalHome {
        source: dhttp::home::LoadDhttpHomeError,
    },
    #[snafu(display("failed to resolve the current directory for pishoo configuration"))]
    CurrentDirectory { source: std::io::Error },
    #[snafu(display("invalid pishoo configuration path {}", path.display()))]
    ConfigPath {
        path: PathBuf,
        source: ResolvedConfigPathError,
    },
    #[snafu(display("invalid global dhttp home path {}", path.display()))]
    DhttpHome {
        path: PathBuf,
        source: ResolvedConfigPathError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigSourceKind {
    GlobalHome,
    ExplicitFile,
}

/// A source selection whose home and configuration path were anchored together exactly once.
///
/// The representation is deliberately opaque: callers cannot construct a relative source or pair
/// one global home with an unrelated configuration path.
///
/// ```compile_fail
/// use pishoo::config::PishooConfigSource;
/// let _ = PishooConfigSource::ExplicitFile { config_path: "relative.conf".into() };
/// ```
#[derive(Debug, Clone)]
pub struct PishooConfigSource {
    kind: ConfigSourceKind,
    home: Option<DhttpHome>,
    config_path: ResolvedConfigPath,
}

impl PishooConfigSource {
    pub const CONFIG_FILE_NAME: &'static str = "pishoo.conf";

    pub fn resolve(config_file: Option<PathBuf>) -> Result<Self, ResolveConfigSourceError> {
        let current_dir =
            std::env::current_dir().context(resolve_config_source_error::CurrentDirectorySnafu)?;
        Self::resolve_at(config_file, &current_dir)
    }

    fn resolve_at(
        config_file: Option<PathBuf>,
        current_dir: &Path,
    ) -> Result<Self, ResolveConfigSourceError> {
        match config_file {
            Some(config_path) => Self::explicit_at(config_path, current_dir),
            None => {
                let home = DhttpHome::load(HomeScope::Global)
                    .context(resolve_config_source_error::GlobalHomeSnafu)?;
                Self::from_global_home_at(home, current_dir)
            }
        }
    }

    pub(crate) fn explicit_at(
        config_path: PathBuf,
        current_dir: &Path,
    ) -> Result<Self, ResolveConfigSourceError> {
        let config_path = anchor(config_path, current_dir);
        let config_path = ResolvedConfigPath::try_from(config_path.clone())
            .context(resolve_config_source_error::ConfigPathSnafu { path: config_path })?;
        Ok(Self {
            kind: ConfigSourceKind::ExplicitFile,
            home: None,
            config_path,
        })
    }

    pub(crate) fn from_global_home_at(
        home: DhttpHome,
        current_dir: &Path,
    ) -> Result<Self, ResolveConfigSourceError> {
        let home_path = anchor(home.as_path().to_path_buf(), current_dir);
        let resolved_home = ResolvedConfigPath::try_from(home_path.clone()).context(
            resolve_config_source_error::DhttpHomeSnafu {
                path: home_path.clone(),
            },
        )?;
        let home = DhttpHome::new(resolved_home.as_ref().to_path_buf());
        let config_path = home.join(Self::CONFIG_FILE_NAME);
        let config_path = ResolvedConfigPath::try_from(config_path.clone())
            .context(resolve_config_source_error::ConfigPathSnafu { path: config_path })?;
        Ok(Self {
            kind: ConfigSourceKind::GlobalHome,
            home: Some(home),
            config_path,
        })
    }

    pub fn config_path(&self) -> &Path {
        self.config_path.as_ref()
    }

    pub fn dhttp_home(&self) -> Option<&DhttpHome> {
        self.home.as_ref()
    }

    pub fn build_options(&self) -> gateway::parse::registry::BuildOptions<'_> {
        gateway::parse::registry::BuildOptions {
            dhttp_home: self.dhttp_home(),
            identity_profile: None,
        }
    }

    pub fn default_worker_groups_enabled(&self) -> bool {
        self.kind == ConfigSourceKind::GlobalHome
    }

    pub fn load_identity_services(&self) -> bool {
        self.kind == ConfigSourceKind::GlobalHome
    }
}

fn anchor(path: PathBuf, current_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        current_dir.join(path)
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
        let source = PishooConfigSource::from_global_home_at(
            home("/tmp/dhttp-global"),
            Path::new("/ignored"),
        )
        .unwrap();

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
        let source = PishooConfigSource::explicit_at(
            PathBuf::from("/tmp/custom.conf"),
            Path::new("/ignored"),
        )
        .unwrap();

        assert_eq!(source.config_path(), Path::new("/tmp/custom.conf"));
        assert!(source.dhttp_home().is_none());
        assert!(!source.default_worker_groups_enabled());
        assert!(!source.load_identity_services());
    }

    #[test]
    fn relative_explicit_source_is_anchored_once_to_absolute_path() {
        let source = PishooConfigSource::explicit_at(
            PathBuf::from("config/pishoo.conf"),
            Path::new("/srv/pishoo"),
        )
        .unwrap();
        assert_eq!(
            source.config_path(),
            Path::new("/srv/pishoo/config/pishoo.conf")
        );
    }

    #[test]
    fn relative_global_home_is_anchored_once_to_absolute_path() {
        let source =
            PishooConfigSource::from_global_home_at(home("state/dhttp"), Path::new("/srv/pishoo"))
                .unwrap();
        assert_eq!(
            source.dhttp_home().unwrap().as_path(),
            Path::new("/srv/pishoo/state/dhttp")
        );
        assert_eq!(
            source.config_path(),
            Path::new("/srv/pishoo/state/dhttp/pishoo.conf")
        );
    }

    #[test]
    fn reload_reuses_resolved_source_after_base_changes() {
        let source =
            PishooConfigSource::explicit_at(PathBuf::from("pishoo.conf"), Path::new("/first"))
                .unwrap();
        let retained = source.clone();
        assert_eq!(retained.config_path(), Path::new("/first/pishoo.conf"));
        assert_ne!(retained.config_path(), Path::new("/second/pishoo.conf"));
    }
}

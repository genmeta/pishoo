use std::{
    fmt,
    path::{Path, PathBuf},
};

use dhttp::home::{DhttpHome, identity::IdentityProfile};
use snafu::{ResultExt, Snafu};

use crate::parse::{registry::BuildOptions, source::SourceSpan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigDocumentId(u32);

impl ConfigDocumentId {
    pub(crate) fn try_from_index(index: u64) -> Result<Self, ConfigDocumentIdError> {
        let index =
            u32::try_from(index).context(config_document_id_error::IndexOverflowSnafu { index })?;
        Ok(Self(index))
    }
}

pub(crate) struct ConfigDocumentIdAllocator {
    next_index: u64,
}

impl ConfigDocumentIdAllocator {
    pub(crate) fn new() -> Self {
        Self { next_index: 0 }
    }

    #[cfg(test)]
    fn with_next_index(next_index: u64) -> Self {
        Self { next_index }
    }

    pub(crate) fn allocate(&mut self) -> Result<ConfigDocumentId, ConfigDocumentIdError> {
        let document_id = ConfigDocumentId::try_from_index(self.next_index)?;
        self.next_index = self
            .next_index
            .checked_add(1)
            .expect("a valid u32 document index always has a u64 successor");
        Ok(document_id)
    }
}

impl fmt::Display for ConfigDocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "document:{}", self.0)
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ConfigDocumentIdError {
    #[snafu(display("configuration document index exceeds the supported range"))]
    IndexOverflow {
        index: u64,
        source: std::num::TryFromIntError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigSourceSpan {
    document_id: ConfigDocumentId,
    span: SourceSpan,
}

impl ConfigSourceSpan {
    pub(crate) fn new(document_id: ConfigDocumentId, span: SourceSpan) -> Self {
        Self { document_id, span }
    }

    pub fn document_id(&self) -> ConfigDocumentId {
        self.document_id
    }

    pub fn start(&self) -> usize {
        self.span.start
    }

    pub fn end(&self) -> usize {
        self.span.end
    }

    pub fn is_empty(&self) -> bool {
        self.span.is_empty()
    }

    pub(crate) fn source_span(&self) -> SourceSpan {
        self.span
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirectiveName(&'static str);

impl DirectiveName {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for DirectiveName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[derive(Debug)]
pub enum ConfigDocumentRole<'a> {
    HypervisorRoot {
        home: Option<&'a DhttpHome>,
    },
    WorkerPishoo {
        home: &'a DhttpHome,
    },
    IdentityServer {
        home: &'a DhttpHome,
        profile: &'a IdentityProfile,
    },
}

impl ConfigDocumentRole<'_> {
    pub(crate) fn kind(&self) -> ConfigDocumentRoleKind {
        match self {
            Self::HypervisorRoot { .. } => ConfigDocumentRoleKind::HypervisorRoot,
            Self::WorkerPishoo { .. } => ConfigDocumentRoleKind::WorkerPishoo,
            Self::IdentityServer { .. } => ConfigDocumentRoleKind::IdentityServer,
        }
    }

    pub(crate) fn build_options(&self) -> BuildOptions<'_> {
        match self {
            Self::HypervisorRoot { home } => BuildOptions {
                dhttp_home: *home,
                identity_profile: None,
            },
            Self::WorkerPishoo { home } => BuildOptions {
                dhttp_home: Some(home),
                identity_profile: None,
            },
            Self::IdentityServer { home, profile } => BuildOptions {
                dhttp_home: Some(home),
                identity_profile: Some(profile),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigDocumentRoleKind {
    HypervisorRoot,
    WorkerPishoo,
    IdentityServer,
}

impl ConfigDocumentRoleKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::HypervisorRoot => "hypervisor root",
            Self::WorkerPishoo => "worker pishoo",
            Self::IdentityServer => "identity server",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedConfigPath(PathBuf);

impl TryFrom<PathBuf> for ResolvedConfigPath {
    type Error = ResolvedConfigPathError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        if !path.is_absolute() {
            return Err(ResolvedConfigPathError::Relative { path });
        }
        if path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(ResolvedConfigPathError::Nul { path });
        }
        Ok(Self(path))
    }
}

impl AsRef<Path> for ResolvedConfigPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl From<ResolvedConfigPath> for PathBuf {
    fn from(path: ResolvedConfigPath) -> Self {
        path.0
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ResolvedConfigPathError {
    #[snafu(display("resolved configuration path must be absolute"))]
    Relative { path: PathBuf },
    #[snafu(display("resolved configuration path contains a NUL byte"))]
    Nul { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_id_allocator_errors_after_allocating_maximum_id() {
        let maximum_index = u64::from(u32::MAX);
        let mut allocator = ConfigDocumentIdAllocator::with_next_index(maximum_index);

        assert_eq!(
            allocator.allocate().expect("maximum id should allocate"),
            ConfigDocumentId::try_from_index(maximum_index)
                .expect("maximum index should be a valid document id")
        );
        let error = allocator
            .allocate()
            .expect_err("allocator must not reuse the maximum id");
        assert!(matches!(
            error,
            ConfigDocumentIdError::IndexOverflow { index, .. }
                if index == maximum_index + 1
        ));
    }
}

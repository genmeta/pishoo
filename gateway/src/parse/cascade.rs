use std::{marker::PhantomData, num::NonZeroU32, path::Path, sync::Arc};

use crate::parse::{
    domain::{ConfigSourceSpan, DirectiveName},
    snapshot::RootConfigSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigOrigin {
    Source(ConfigSourceSpan),
    RootInherited {
        directive: DirectiveName,
        source: Option<InheritedSourceLocation>,
    },
    Builtin {
        directive: DirectiveName,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedSourceLocation {
    document: Option<std::path::PathBuf>,
    line: NonZeroU32,
    column: NonZeroU32,
}

impl InheritedSourceLocation {
    pub(crate) fn new(
        document: Option<std::path::PathBuf>,
        line: NonZeroU32,
        column: NonZeroU32,
    ) -> Self {
        Self {
            document,
            line,
            column,
        }
    }

    pub fn document(&self) -> Option<&Path> {
        self.document.as_deref()
    }

    pub const fn line(&self) -> NonZeroU32 {
        self.line
    }

    pub const fn column(&self) -> NonZeroU32 {
        self.column
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadedValue<T> {
    effective: T,
    lineage: Box<[ConfigOrigin]>,
}

impl<T> CascadedValue<T> {
    pub(crate) fn new(effective: T, lineage: Box<[ConfigOrigin]>) -> Self {
        Self { effective, lineage }
    }

    pub fn effective(&self) -> &T {
        &self.effective
    }

    pub fn lineage(&self) -> &[ConfigOrigin] {
        &self.lineage
    }
}

pub(crate) type BuiltinValue<T> = fn() -> Option<Arc<T>>;
pub(crate) type SnapshotValue<T> =
    fn(&RootConfigSnapshot, DirectiveName) -> Option<(Arc<T>, ConfigOrigin)>;

#[derive(Debug)]
pub struct DirectiveKey<T> {
    name: DirectiveName,
    builtin: BuiltinValue<T>,
    snapshot: SnapshotValue<T>,
    value: PhantomData<fn() -> T>,
}

impl<T> Clone for DirectiveKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for DirectiveKey<T> {}

impl<T> DirectiveKey<T> {
    pub(crate) const fn new(
        name: &'static str,
        builtin: BuiltinValue<T>,
        snapshot: SnapshotValue<T>,
    ) -> Self {
        Self {
            name: DirectiveName::new(name),
            builtin,
            snapshot,
            value: PhantomData,
        }
    }

    pub const fn name(self) -> DirectiveName {
        self.name
    }

    pub(crate) fn builtin(self) -> Option<Arc<T>> {
        (self.builtin)()
    }

    pub(crate) fn snapshot(self, snapshot: &RootConfigSnapshot) -> Option<(Arc<T>, ConfigOrigin)> {
        (self.snapshot)(snapshot, self.name)
    }
}

pub(crate) fn absent<T>() -> Option<Arc<T>> {
    None
}

pub(crate) fn no_snapshot<T>(
    _snapshot: &RootConfigSnapshot,
    _name: DirectiveName,
) -> Option<(Arc<T>, ConfigOrigin)> {
    None
}

pub(crate) fn builtin_false() -> Option<Arc<crate::parse::types::BoolConfig>> {
    Some(Arc::new(crate::parse::types::BoolConfig(false)))
}

pub(crate) fn builtin_min_length() -> Option<Arc<crate::parse::types::GzipMinLength>> {
    Some(Arc::new(crate::parse::types::GzipMinLength::checked(20)))
}

pub(crate) fn builtin_comp_level() -> Option<Arc<crate::parse::types::GzipCompLevel>> {
    Some(Arc::new(crate::parse::types::GzipCompLevel::checked(1)))
}

#[cfg(test)]
pub(crate) const GZIP: DirectiveKey<crate::parse::types::BoolConfig> =
    crate::parse::registry::v1_gzip_key();
#[cfg(test)]
pub(crate) const GZIP_TYPES: DirectiveKey<crate::parse::types::StringList> =
    crate::parse::registry::v1_gzip_types_key();
#[cfg(test)]
pub(crate) const TYPES: DirectiveKey<crate::parse::types::MimeTypes> =
    crate::parse::registry::v1_types_key();

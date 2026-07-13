use std::{marker::PhantomData, num::NonZeroU32, path::Path, sync::Arc};

use crate::parse::{
    domain::{ConfigSourceSpan, DirectiveName},
    snapshot::RootConfigSnapshot,
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, MimeTypes,
        StringList,
    },
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

type BuiltinValue<T> = fn() -> Option<Arc<T>>;
type SnapshotValue<T> = fn(&RootConfigSnapshot, DirectiveName) -> Option<(Arc<T>, ConfigOrigin)>;

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

fn absent<T>() -> Option<Arc<T>> {
    None
}

fn builtin_false() -> Option<Arc<BoolConfig>> {
    Some(Arc::new(BoolConfig(false)))
}

fn builtin_min_length() -> Option<Arc<GzipMinLength>> {
    Some(Arc::new(GzipMinLength::checked(20)))
}

fn builtin_comp_level() -> Option<Arc<GzipCompLevel>> {
    Some(Arc::new(GzipCompLevel::checked(1)))
}

pub const ACCESS_RULES: DirectiveKey<AccessRulesUri> = DirectiveKey::new(
    "access_rules",
    absent,
    RootConfigSnapshot::cascade_access_rules,
);
pub const GZIP: DirectiveKey<BoolConfig> =
    DirectiveKey::new("gzip", builtin_false, RootConfigSnapshot::cascade_gzip);
pub const GZIP_VARY: DirectiveKey<BoolConfig> = DirectiveKey::new(
    "gzip_vary",
    builtin_false,
    RootConfigSnapshot::cascade_gzip_vary,
);
pub const GZIP_MIN_LENGTH: DirectiveKey<GzipMinLength> = DirectiveKey::new(
    "gzip_min_length",
    builtin_min_length,
    RootConfigSnapshot::cascade_gzip_min_length,
);
pub const GZIP_COMP_LEVEL: DirectiveKey<GzipCompLevel> = DirectiveKey::new(
    "gzip_comp_level",
    builtin_comp_level,
    RootConfigSnapshot::cascade_gzip_comp_level,
);
pub const GZIP_TYPES: DirectiveKey<StringList> =
    DirectiveKey::new("gzip_types", absent, RootConfigSnapshot::cascade_gzip_types);
pub const DEFAULT_TYPE: DirectiveKey<DefaultType> = DirectiveKey::new(
    "default_type",
    absent,
    RootConfigSnapshot::cascade_default_type,
);
pub const TYPES: DirectiveKey<MimeTypes> =
    DirectiveKey::new("types", absent, RootConfigSnapshot::cascade_types);

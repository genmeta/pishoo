use std::{collections::HashSet, num::NonZeroU32, path::PathBuf, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _};
use snafu::Snafu;

use crate::parse::{
    cascade::{
        ACCESS_RULES, ConfigOrigin, DEFAULT_TYPE, GZIP, GZIP_COMP_LEVEL, GZIP_MIN_LENGTH,
        GZIP_TYPES, GZIP_VARY, InheritedSourceLocation, TYPES,
    },
    domain::{DirectiveName, ResolvedConfigPath, ResolvedConfigPathError},
    tree::HomeConfigTree,
    types::{
        AccessRulesUri, AccessRulesUriValidationError, BoolConfig, DefaultType, GzipCompLevel,
        GzipMinLength, GzipTypesValidationError, MimeTypes, MimeTypesValidationError, StringList,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootConfigSnapshot {
    V1(RootInheritedConfigV1),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInheritedConfigV1 {
    access_rules: InheritedValue<Option<AccessRulesUri>>,
    gzip: InheritedValue<BoolConfig>,
    gzip_vary: InheritedValue<BoolConfig>,
    gzip_min_length: InheritedValue<GzipMinLength>,
    gzip_comp_level: InheritedValue<GzipCompLevel>,
    gzip_types: InheritedValue<Option<StringList>>,
    default_type: InheritedValue<Option<DefaultType>>,
    types: InheritedValue<Option<MimeTypes>>,
    access_log: InheritedValue<Option<SnapshotAccessLogDirective>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InheritedValue<T> {
    value: T,
    origin: InheritedOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InheritedOrigin {
    Builtin,
    Source(InheritedSourceLocation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotAccessLogDirective {
    Off,
    ProfileDefault,
    Resolved(ResolvedConfigPath),
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RootConfigSnapshotError {
    #[snafu(display("configuration source path must be absolute for root snapshot transport"))]
    SourcePath { path: PathBuf },
    #[snafu(display("configuration source location exceeds the root snapshot range"))]
    SourceLocation { line: usize, column: usize },
    #[snafu(display("root configuration value has no source location"))]
    MissingSourceLocation,
    #[snafu(display("required root snapshot fallback is missing for `{directive}`"))]
    MissingFallback { directive: DirectiveName },
    #[snafu(display("failed to query root configuration value"))]
    Query {
        source: crate::parse::error::ConfigQueryError,
    },
    #[snafu(display("invalid root snapshot access_rules value"))]
    AccessRules {
        source: AccessRulesUriValidationError,
    },
    #[snafu(display("invalid root snapshot header value"))]
    HeaderValue {
        source: http::header::InvalidHeaderValue,
    },
    #[snafu(display("invalid root snapshot gzip_types value"))]
    GzipTypes { source: GzipTypesValidationError },
    #[snafu(display("invalid root snapshot MIME types value"))]
    MimeTypes { source: MimeTypesValidationError },
    #[snafu(display("invalid root snapshot absolute path"))]
    AbsolutePath { path: PathBuf },
    #[snafu(display("root snapshot path transport requires a Unix host"))]
    UnsupportedPathPlatform,
    #[snafu(display("invalid resolved root snapshot path"))]
    ResolvedPath { source: ResolvedConfigPathError },
}

impl RootConfigSnapshot {
    pub(crate) fn project(tree: &Arc<HomeConfigTree>) -> Result<Self, RootConfigSnapshotError> {
        let pishoo = tree.pishoo();
        let access_rules = project_optional(tree, snapshot_query(pishoo.cascaded(ACCESS_RULES))?)?;
        let gzip = project_required(tree, GZIP.name(), snapshot_query(pishoo.cascaded(GZIP))?)?;
        let gzip_vary = project_required(
            tree,
            GZIP_VARY.name(),
            snapshot_query(pishoo.cascaded(GZIP_VARY))?,
        )?;
        let gzip_min_length = project_required(
            tree,
            GZIP_MIN_LENGTH.name(),
            snapshot_query(pishoo.cascaded(GZIP_MIN_LENGTH))?,
        )?;
        let gzip_comp_level = project_required(
            tree,
            GZIP_COMP_LEVEL.name(),
            snapshot_query(pishoo.cascaded(GZIP_COMP_LEVEL))?,
        )?;
        let gzip_types = project_optional(tree, snapshot_query(pishoo.cascaded(GZIP_TYPES))?)?;
        let default_type = project_optional(tree, snapshot_query(pishoo.cascaded(DEFAULT_TYPE))?)?;
        let types = project_optional(tree, snapshot_query(pishoo.cascaded(TYPES))?)?;
        Ok(Self::V1(RootInheritedConfigV1 {
            access_rules,
            gzip,
            gzip_vary,
            gzip_min_length,
            gzip_comp_level,
            gzip_types,
            default_type,
            types,
            access_log: InheritedValue {
                value: None,
                origin: InheritedOrigin::Builtin,
            },
        }))
    }

    pub fn access_rules(&self) -> Option<&AccessRulesUri> {
        self.v1().access_rules.value.as_ref()
    }

    pub fn gzip(&self) -> &BoolConfig {
        &self.v1().gzip.value
    }

    pub fn gzip_vary(&self) -> &BoolConfig {
        &self.v1().gzip_vary.value
    }

    pub fn gzip_min_length(&self) -> &GzipMinLength {
        &self.v1().gzip_min_length.value
    }

    pub fn gzip_comp_level(&self) -> &GzipCompLevel {
        &self.v1().gzip_comp_level.value
    }

    pub fn gzip_types(&self) -> Option<&StringList> {
        self.v1().gzip_types.value.as_ref()
    }

    pub fn default_type(&self) -> Option<&DefaultType> {
        self.v1().default_type.value.as_ref()
    }

    pub fn types(&self) -> Option<&MimeTypes> {
        self.v1().types.value.as_ref()
    }

    pub(crate) fn cascade_access_rules(
        &self,
        directive: DirectiveName,
    ) -> Option<(Arc<AccessRulesUri>, ConfigOrigin)> {
        cascade_optional(&self.v1().access_rules, directive)
    }

    pub(crate) fn cascade_gzip(
        &self,
        directive: DirectiveName,
    ) -> Option<(Arc<BoolConfig>, ConfigOrigin)> {
        cascade_required(&self.v1().gzip, directive)
    }

    pub(crate) fn cascade_gzip_vary(
        &self,
        directive: DirectiveName,
    ) -> Option<(Arc<BoolConfig>, ConfigOrigin)> {
        cascade_required(&self.v1().gzip_vary, directive)
    }

    pub(crate) fn cascade_gzip_min_length(
        &self,
        directive: DirectiveName,
    ) -> Option<(Arc<GzipMinLength>, ConfigOrigin)> {
        cascade_required(&self.v1().gzip_min_length, directive)
    }

    pub(crate) fn cascade_gzip_comp_level(
        &self,
        directive: DirectiveName,
    ) -> Option<(Arc<GzipCompLevel>, ConfigOrigin)> {
        cascade_required(&self.v1().gzip_comp_level, directive)
    }

    pub(crate) fn cascade_gzip_types(
        &self,
        directive: DirectiveName,
    ) -> Option<(Arc<StringList>, ConfigOrigin)> {
        cascade_optional(&self.v1().gzip_types, directive)
    }

    pub(crate) fn cascade_default_type(
        &self,
        directive: DirectiveName,
    ) -> Option<(Arc<DefaultType>, ConfigOrigin)> {
        cascade_optional(&self.v1().default_type, directive)
    }

    pub(crate) fn cascade_types(
        &self,
        directive: DirectiveName,
    ) -> Option<(Arc<MimeTypes>, ConfigOrigin)> {
        cascade_optional(&self.v1().types, directive)
    }

    fn v1(&self) -> &RootInheritedConfigV1 {
        match self {
            Self::V1(value) => value,
        }
    }
}

fn snapshot_query<T>(
    result: Result<
        Option<crate::parse::cascade::CascadedValue<Arc<T>>>,
        crate::parse::error::ConfigQueryError,
    >,
) -> Result<Option<crate::parse::cascade::CascadedValue<Arc<T>>>, RootConfigSnapshotError> {
    result.map_err(|source| RootConfigSnapshotError::Query { source })
}

fn project_required<T: Clone>(
    tree: &HomeConfigTree,
    directive: DirectiveName,
    value: Option<crate::parse::cascade::CascadedValue<Arc<T>>>,
) -> Result<InheritedValue<T>, RootConfigSnapshotError> {
    let value = value.ok_or(RootConfigSnapshotError::MissingFallback { directive })?;
    let origin = project_origin(tree, value.lineage().last())?;
    Ok(InheritedValue {
        value: value.effective().as_ref().clone(),
        origin,
    })
}

fn project_optional<T: Clone>(
    tree: &HomeConfigTree,
    value: Option<crate::parse::cascade::CascadedValue<Arc<T>>>,
) -> Result<InheritedValue<Option<T>>, RootConfigSnapshotError> {
    let Some(value) = value else {
        return Ok(InheritedValue {
            value: None,
            origin: InheritedOrigin::Builtin,
        });
    };
    let origin = project_origin(tree, value.lineage().last())?;
    Ok(InheritedValue {
        value: Some(value.effective().as_ref().clone()),
        origin,
    })
}

fn project_origin(
    tree: &HomeConfigTree,
    origin: Option<&ConfigOrigin>,
) -> Result<InheritedOrigin, RootConfigSnapshotError> {
    match origin {
        None | Some(ConfigOrigin::Builtin { .. }) => Ok(InheritedOrigin::Builtin),
        Some(ConfigOrigin::Source(span)) => tree
            .inherited_source_location(*span)
            .map(InheritedOrigin::Source),
        Some(ConfigOrigin::RootInherited { source, .. }) => Ok(source
            .clone()
            .map_or(InheritedOrigin::Builtin, InheritedOrigin::Source)),
    }
}

fn cascade_required<T: Clone>(
    value: &InheritedValue<T>,
    directive: DirectiveName,
) -> Option<(Arc<T>, ConfigOrigin)> {
    Some((
        Arc::new(value.value.clone()),
        inherited_origin(&value.origin, directive),
    ))
}

fn cascade_optional<T: Clone>(
    value: &InheritedValue<Option<T>>,
    directive: DirectiveName,
) -> Option<(Arc<T>, ConfigOrigin)> {
    let origin = &value.origin;
    value
        .value
        .as_ref()
        .map(|value| (Arc::new(value.clone()), inherited_origin(origin, directive)))
}

fn inherited_origin(origin: &InheritedOrigin, directive: DirectiveName) -> ConfigOrigin {
    match origin {
        InheritedOrigin::Builtin => ConfigOrigin::Builtin { directive },
        InheritedOrigin::Source(source) => ConfigOrigin::RootInherited {
            directive,
            source: Some(source.clone()),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum RootConfigSnapshotWire {
    V1(RootInheritedConfigV1Wire),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RootInheritedConfigV1Wire {
    access_rules: InheritedValueV1<Option<AccessRulesSourceV1>>,
    gzip: InheritedValueV1<bool>,
    gzip_vary: InheritedValueV1<bool>,
    gzip_min_length: InheritedValueV1<u64>,
    gzip_comp_level: InheritedValueV1<i32>,
    gzip_types: InheritedValueV1<Option<Box<[Box<str>]>>>,
    default_type: InheritedValueV1<Option<HeaderValueV1>>,
    types: InheritedValueV1<Option<MimeTypesV1>>,
    access_log: InheritedValueV1<Option<AccessLogDirectiveV1>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InheritedValueV1<T> {
    value: T,
    origin: InheritedOriginV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum InheritedOriginV1 {
    Builtin,
    Source(InheritedSourceLocationV1),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InheritedSourceLocationV1 {
    document: Option<AbsolutePathV1>,
    line: NonZeroU32,
    column: NonZeroU32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HeaderValueV1(Box<[u8]>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MimeTypeEntryV1 {
    extension: Box<str>,
    value: HeaderValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MimeTypesV1(Box<[MimeTypeEntryV1]>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AccessRulesSourceV1(Box<str>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AbsolutePathV1(Box<[u8]>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResolvedConfigPathV1(AbsolutePathV1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum AccessLogDirectiveV1 {
    Off,
    ProfileDefault,
    Resolved(ResolvedConfigPathV1),
}

impl Serialize for RootConfigSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = RootConfigSnapshotWire::try_from(self).map_err(S::Error::custom)?;
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RootConfigSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RootConfigSnapshotWire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(D::Error::custom)
    }
}

impl TryFrom<&RootConfigSnapshot> for RootConfigSnapshotWire {
    type Error = RootConfigSnapshotError;

    fn try_from(snapshot: &RootConfigSnapshot) -> Result<Self, Self::Error> {
        let value = snapshot.v1();
        Ok(Self::V1(RootInheritedConfigV1Wire {
            access_rules: encode_inherited(&value.access_rules, |value| {
                value
                    .as_ref()
                    .map(|value| AccessRulesSourceV1(value.0.as_str().into()))
            })?,
            gzip: encode_inherited(&value.gzip, |value| value.0)?,
            gzip_vary: encode_inherited(&value.gzip_vary, |value| value.0)?,
            gzip_min_length: encode_inherited(&value.gzip_min_length, |value| value.0)?,
            gzip_comp_level: encode_inherited(&value.gzip_comp_level, |value| value.0)?,
            gzip_types: encode_inherited(&value.gzip_types, |value| {
                value
                    .as_ref()
                    .map(|value| value.0.iter().map(|value| value.as_str().into()).collect())
            })?,
            default_type: encode_inherited(&value.default_type, |value| {
                value
                    .as_ref()
                    .map(|value| HeaderValueV1(value.0.as_bytes().into()))
            })?,
            types: encode_inherited(&value.types, |value| value.as_ref().map(encode_mime_types))?,
            access_log: encode_inherited(&value.access_log, |value| {
                value.as_ref().map(encode_access_log)
            })?,
        }))
    }
}

impl TryFrom<RootConfigSnapshotWire> for RootConfigSnapshot {
    type Error = RootConfigSnapshotError;

    fn try_from(wire: RootConfigSnapshotWire) -> Result<Self, Self::Error> {
        let RootConfigSnapshotWire::V1(value) = wire;
        Ok(Self::V1(RootInheritedConfigV1 {
            access_rules: decode_inherited(value.access_rules, |value| {
                value
                    .map(|value| {
                        let uri = url::Url::parse(&value.0).map_err(|_| {
                            RootConfigSnapshotError::AccessRules {
                                source: AccessRulesUriValidationError::UnsupportedSqliteForm,
                            }
                        })?;
                        AccessRulesUri::try_from(uri)
                            .map_err(|source| RootConfigSnapshotError::AccessRules { source })
                    })
                    .transpose()
            })?,
            gzip: decode_inherited(value.gzip, |value| Ok(BoolConfig(value)))?,
            gzip_vary: decode_inherited(value.gzip_vary, |value| Ok(BoolConfig(value)))?,
            gzip_min_length: decode_inherited(value.gzip_min_length, |value| {
                Ok(GzipMinLength::checked(value))
            })?,
            gzip_comp_level: decode_inherited(value.gzip_comp_level, |value| {
                Ok(GzipCompLevel::checked(value))
            })?,
            gzip_types: decode_inherited(value.gzip_types, |value| {
                value
                    .map(|values| {
                        StringList::checked_gzip_types(
                            values.into_vec().into_iter().map(String::from).collect(),
                        )
                        .map_err(|source| RootConfigSnapshotError::GzipTypes { source })
                    })
                    .transpose()
            })?,
            default_type: decode_inherited(value.default_type, |value| {
                value
                    .map(|value| {
                        DefaultType::checked_from_bytes(&value.0)
                            .map_err(|source| RootConfigSnapshotError::HeaderValue { source })
                    })
                    .transpose()
            })?,
            types: decode_inherited(value.types, |value| {
                value.map(decode_mime_types).transpose()
            })?,
            access_log: decode_inherited(value.access_log, |value| {
                value.map(decode_access_log).transpose()
            })?,
        }))
    }
}

fn encode_inherited<T, W>(
    value: &InheritedValue<T>,
    encode: impl FnOnce(&T) -> W,
) -> Result<InheritedValueV1<W>, RootConfigSnapshotError> {
    Ok(InheritedValueV1 {
        value: encode(&value.value),
        origin: encode_origin(&value.origin)?,
    })
}

fn decode_inherited<T, W>(
    value: InheritedValueV1<W>,
    decode: impl FnOnce(W) -> Result<T, RootConfigSnapshotError>,
) -> Result<InheritedValue<T>, RootConfigSnapshotError> {
    Ok(InheritedValue {
        value: decode(value.value)?,
        origin: decode_origin(value.origin)?,
    })
}

fn encode_origin(origin: &InheritedOrigin) -> Result<InheritedOriginV1, RootConfigSnapshotError> {
    match origin {
        InheritedOrigin::Builtin => Ok(InheritedOriginV1::Builtin),
        InheritedOrigin::Source(source) => {
            Ok(InheritedOriginV1::Source(InheritedSourceLocationV1 {
                document: source
                    .document()
                    .map(AbsolutePathV1::try_from)
                    .transpose()?,
                line: source.line(),
                column: source.column(),
            }))
        }
    }
}

fn decode_origin(origin: InheritedOriginV1) -> Result<InheritedOrigin, RootConfigSnapshotError> {
    match origin {
        InheritedOriginV1::Builtin => Ok(InheritedOrigin::Builtin),
        InheritedOriginV1::Source(source) => {
            Ok(InheritedOrigin::Source(InheritedSourceLocation::new(
                source.document.map(PathBuf::try_from).transpose()?,
                source.line,
                source.column,
            )))
        }
    }
}

fn encode_mime_types(types: &MimeTypes) -> MimeTypesV1 {
    let mut entries = types
        .0
        .iter()
        .map(|(extension, value)| MimeTypeEntryV1 {
            extension: extension.as_str().into(),
            value: HeaderValueV1(value.as_bytes().into()),
        })
        .collect::<Vec<_>>();
    entries
        .sort_unstable_by(|left, right| left.extension.as_bytes().cmp(right.extension.as_bytes()));
    MimeTypesV1(entries.into_boxed_slice())
}

fn decode_mime_types(types: MimeTypesV1) -> Result<MimeTypes, RootConfigSnapshotError> {
    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(types.0.len());
    for entry in types.0 {
        let extension = String::from(entry.extension);
        if !seen.insert(extension.clone()) {
            return Err(RootConfigSnapshotError::MimeTypes {
                source: MimeTypesValidationError::DuplicateExtension { extension },
            });
        }
        entries.push((extension, entry.value.0.into_vec()));
    }
    MimeTypes::checked_from_bytes(entries)
        .map_err(|source| RootConfigSnapshotError::MimeTypes { source })
}

fn encode_access_log(value: &SnapshotAccessLogDirective) -> AccessLogDirectiveV1 {
    match value {
        SnapshotAccessLogDirective::Off => AccessLogDirectiveV1::Off,
        SnapshotAccessLogDirective::ProfileDefault => AccessLogDirectiveV1::ProfileDefault,
        SnapshotAccessLogDirective::Resolved(path) => {
            AccessLogDirectiveV1::Resolved(ResolvedConfigPathV1(
                AbsolutePathV1::try_from(path.as_ref())
                    .expect("resolved configuration paths are checked when constructed"),
            ))
        }
    }
}

fn decode_access_log(
    value: AccessLogDirectiveV1,
) -> Result<SnapshotAccessLogDirective, RootConfigSnapshotError> {
    Ok(match value {
        AccessLogDirectiveV1::Off => SnapshotAccessLogDirective::Off,
        AccessLogDirectiveV1::ProfileDefault => SnapshotAccessLogDirective::ProfileDefault,
        AccessLogDirectiveV1::Resolved(path) => SnapshotAccessLogDirective::Resolved(
            ResolvedConfigPath::try_from(PathBuf::try_from(path.0)?)
                .map_err(|source| RootConfigSnapshotError::ResolvedPath { source })?,
        ),
    })
}

impl TryFrom<&std::path::Path> for AbsolutePathV1 {
    type Error = RootConfigSnapshotError;

    fn try_from(path: &std::path::Path) -> Result<Self, Self::Error> {
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(RootConfigSnapshotError::AbsolutePath {
                path: path.to_path_buf(),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(Self(path.as_os_str().as_bytes().into()))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(RootConfigSnapshotError::UnsupportedPathPlatform)
        }
    }
}

impl TryFrom<AbsolutePathV1> for PathBuf {
    type Error = RootConfigSnapshotError;

    fn try_from(path: AbsolutePathV1) -> Result<Self, Self::Error> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let path = PathBuf::from(std::ffi::OsString::from_vec(path.0.into_vec()));
            if !path.is_absolute()
                || path.as_os_str().as_encoded_bytes().is_empty()
                || path.as_os_str().as_encoded_bytes().contains(&0)
            {
                return Err(RootConfigSnapshotError::AbsolutePath { path });
            }
            Ok(path)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(RootConfigSnapshotError::UnsupportedPathPlatform)
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct SnapshotValues {
        access_rules: Option<AccessRulesUri>,
        gzip: BoolConfig,
        gzip_vary: BoolConfig,
        gzip_min_length: GzipMinLength,
        gzip_comp_level: GzipCompLevel,
        gzip_types: Option<StringList>,
        default_type: Option<DefaultType>,
        types: Option<MimeTypes>,
        access_log: Option<SnapshotAccessLogDirective>,
    }

    pub(crate) fn wire_field_names(snapshot: &RootConfigSnapshot) -> [&'static str; 9] {
        let RootConfigSnapshotWire::V1(RootInheritedConfigV1Wire {
            access_rules,
            gzip,
            gzip_vary,
            gzip_min_length,
            gzip_comp_level,
            gzip_types,
            default_type,
            types,
            access_log,
        }) = RootConfigSnapshotWire::try_from(snapshot).expect("snapshot should encode");
        drop((
            access_rules,
            gzip,
            gzip_vary,
            gzip_min_length,
            gzip_comp_level,
            gzip_types,
            default_type,
            types,
            access_log,
        ));
        [
            "access_rules",
            "gzip",
            "gzip_vary",
            "gzip_min_length",
            "gzip_comp_level",
            "gzip_types",
            "default_type",
            "types",
            "access_log",
        ]
    }

    pub(crate) fn checked_wire_round_trip(
        snapshot: &RootConfigSnapshot,
    ) -> Result<RootConfigSnapshot, RootConfigSnapshotError> {
        RootConfigSnapshot::try_from(RootConfigSnapshotWire::try_from(snapshot)?)
    }

    pub(crate) fn snapshot_with_builtin_gzip(gzip: bool) -> RootConfigSnapshot {
        let builtin = || InheritedOrigin::Builtin;
        RootConfigSnapshot::V1(RootInheritedConfigV1 {
            access_rules: InheritedValue {
                value: None,
                origin: builtin(),
            },
            gzip: InheritedValue {
                value: BoolConfig(gzip),
                origin: builtin(),
            },
            gzip_vary: InheritedValue {
                value: BoolConfig(false),
                origin: builtin(),
            },
            gzip_min_length: InheritedValue {
                value: GzipMinLength::checked(20),
                origin: builtin(),
            },
            gzip_comp_level: InheritedValue {
                value: GzipCompLevel::checked(1),
                origin: builtin(),
            },
            gzip_types: InheritedValue {
                value: None,
                origin: builtin(),
            },
            default_type: InheritedValue {
                value: None,
                origin: builtin(),
            },
            types: InheritedValue {
                value: None,
                origin: builtin(),
            },
            access_log: InheritedValue {
                value: None,
                origin: builtin(),
            },
        })
    }

    pub(crate) fn values_without_origins(snapshot: &RootConfigSnapshot) -> SnapshotValues {
        let value = snapshot.v1();
        SnapshotValues {
            access_rules: value.access_rules.value.clone(),
            gzip: value.gzip.value.clone(),
            gzip_vary: value.gzip_vary.value.clone(),
            gzip_min_length: value.gzip_min_length.value.clone(),
            gzip_comp_level: value.gzip_comp_level.value.clone(),
            gzip_types: value.gzip_types.value.clone(),
            default_type: value.default_type.value.clone(),
            types: value.types.value.clone(),
            access_log: value.access_log.value.clone(),
        }
    }

    pub(crate) fn round_trip_resolved_path(
        path: ResolvedConfigPath,
    ) -> Result<ResolvedConfigPath, RootConfigSnapshotError> {
        let wire = ResolvedConfigPathV1(AbsolutePathV1::try_from(path.as_ref())?);
        ResolvedConfigPath::try_from(PathBuf::try_from(wire.0)?)
            .map_err(|source| RootConfigSnapshotError::ResolvedPath { source })
    }

    pub(crate) fn decode_absolute_path(bytes: &[u8]) -> Result<PathBuf, RootConfigSnapshotError> {
        PathBuf::try_from(AbsolutePathV1(bytes.into()))
    }

    pub(crate) fn decode_schema(schema: u32) -> Result<(), serde::de::value::Error> {
        let variant = format!("V{schema}");
        let deserializer = serde::de::value::StringDeserializer::new(variant);
        RootConfigSnapshot::deserialize(deserializer).map(drop)
    }

    pub(crate) fn decode_access_rules(
        value: &str,
    ) -> Result<AccessRulesUri, RootConfigSnapshotError> {
        let uri = url::Url::parse(value).map_err(|_| RootConfigSnapshotError::AccessRules {
            source: AccessRulesUriValidationError::UnsupportedSqliteForm,
        })?;
        AccessRulesUri::try_from(uri)
            .map_err(|source| RootConfigSnapshotError::AccessRules { source })
    }

    pub(crate) fn decode_header_value(
        value: &[u8],
    ) -> Result<DefaultType, RootConfigSnapshotError> {
        DefaultType::checked_from_bytes(value)
            .map_err(|source| RootConfigSnapshotError::HeaderValue { source })
    }

    pub(crate) fn mime_wire_extensions(types: &MimeTypes) -> Vec<String> {
        let wire = encode_mime_types(types);
        wire.0
            .iter()
            .map(|entry| entry.extension.to_string())
            .collect()
    }

    pub(crate) fn decode_mime_entries(
        entries: &[(&str, &[u8])],
    ) -> Result<MimeTypes, RootConfigSnapshotError> {
        decode_mime_types(MimeTypesV1(
            entries
                .iter()
                .map(|(extension, value)| MimeTypeEntryV1 {
                    extension: (*extension).into(),
                    value: HeaderValueV1((*value).into()),
                })
                .collect(),
        ))
    }

    pub(crate) fn has_access_log(snapshot: &RootConfigSnapshot) -> bool {
        snapshot.v1().access_log.value.is_some()
    }
}

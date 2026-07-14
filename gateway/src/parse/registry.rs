use std::{any::TypeId, collections::HashMap, error::Error, sync::Arc};

use snafu::{OptionExt, ResultExt, Snafu};

use crate::parse::{
    ast::{AstBody, AstDirective},
    cascade::{BuiltinValue, DirectiveKey, SnapshotValue},
    document::{ConfigDocument, ConfigNode},
    domain::{ConfigDocumentRoleKind, DirectiveName},
    error::{BuildDocumentError, ConfigDocumentRoleError, build_document_error},
    fragment::{ParsedConfigDocument, ParsedPishooFragment, ParsedServerFragment},
    normalize,
    snapshot::RootConfigSnapshot,
    source::{ConfigDocumentSourceMap, SourceMap, SourceSpan},
    tree::AttachedConfigNode,
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, MimeTypes,
        StringList,
    },
    value::{ConfigValue, TypedValue},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextKey(pub &'static str);

impl std::fmt::Display for ContextKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

pub mod context {
    use super::ContextKey;

    pub const ROOT: ContextKey = ContextKey("gateway.root");
    pub const PISHOO: ContextKey = ContextKey("gateway.pishoo");
    pub const SERVER: ContextKey = ContextKey("gateway.server");
    pub const LOCATION: ContextKey = ContextKey("gateway.location");
    pub const PROXY: ContextKey = ContextKey("gateway.proxy");
}

#[derive(Debug, Default)]
struct ConfigRegistryContractIdentity;

#[derive(Debug, Clone, Default)]
pub(crate) struct ConfigRegistryContract(Arc<ConfigRegistryContractIdentity>);

impl ConfigRegistryContract {
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Default)]
pub struct ConfigRegistry {
    contexts: HashMap<ContextKey, ContextSpec>,
    directives: HashMap<(ContextKey, &'static str), DirectiveSpec>,
    contract_tables: HashMap<ContextKey, Arc<[DirectiveContract]>>,
    attached_finalizers: HashMap<ContextKey, AttachedContextFinalizeFn>,
    contract: ConfigRegistryContract,
}

pub struct ContextSpec {
    pub key: ContextKey,
    pub finalize: Option<ContextFinalizeFn>,
}

pub struct DirectiveSpec {
    pub name: DirectiveName,
    pub allowed_in: Vec<ContextKey>,
    pub shape: DirectiveShape,
    parser: DirectiveParserFn,
    pub duplicate: DuplicatePolicy,
    pub cascade: CascadePolicy,
    pub transport: TransportPolicy,
    pub reload: ReloadImpact,
    value_type: Option<TypeId>,
    value_type_name: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveShape {
    Leaf,
    ContextBlock {
        child_context: ContextKey,
        payload: PayloadMode,
    },
    RawBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadMode {
    None,
    Parser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicatePolicy {
    Reject,
    LastWins,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadePolicy {
    None,
    NearestWins,
    ReplaceWhole,
    MergeByKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPolicy {
    HypervisorOnly,
    WorkerInheritable,
    WorkerLocalOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadImpact {
    Supervisor,
    ListenerSet,
    RuntimeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveCardinality {
    Single,
    Repeated,
    ContextPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveContractMismatch {
    Name {
        expected: DirectiveName,
        actual: Option<DirectiveName>,
    },
    Context {
        expected: ContextKey,
        actual: ContextKey,
    },
    ValueType {
        expected: &'static str,
        actual: &'static str,
    },
    Cardinality {
        expected: DirectiveCardinality,
        actual: DirectiveCardinality,
    },
    Shape {
        expected: DirectiveShape,
        actual: DirectiveShape,
    },
    Payload {
        expected: PayloadMode,
        actual: PayloadMode,
    },
    Duplicate {
        expected: DuplicatePolicy,
        actual: DuplicatePolicy,
    },
    Cascade {
        expected: CascadePolicy,
        actual: CascadePolicy,
    },
}

impl std::fmt::Display for DirectiveContractMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name { expected, actual } => write!(
                f,
                "name expected `{}`, actual {}",
                expected.as_str(),
                actual.map_or("<missing>", |actual| actual.as_str())
            ),
            Self::Context { expected, actual } => {
                write!(f, "context expected `{expected}`, actual `{actual}`")
            }
            Self::ValueType { expected, actual } => {
                write!(f, "value type expected `{expected}`, actual `{actual}`")
            }
            Self::Cardinality { expected, actual } => {
                write!(f, "cardinality expected {expected:?}, actual {actual:?}")
            }
            Self::Shape { expected, actual } => {
                write!(f, "shape expected {expected:?}, actual {actual:?}")
            }
            Self::Payload { expected, actual } => {
                write!(f, "payload expected {expected:?}, actual {actual:?}")
            }
            Self::Duplicate { expected, actual } => {
                write!(
                    f,
                    "duplicate policy expected {expected:?}, actual {actual:?}"
                )
            }
            Self::Cascade { expected, actual } => {
                write!(f, "cascade policy expected {expected:?}, actual {actual:?}")
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectiveContractTemplate {
    context: ContextKey,
    name: DirectiveName,
    value_type: fn() -> TypeId,
    value_type_name: fn() -> &'static str,
    cardinality: DirectiveCardinality,
    shape: DirectiveShape,
    duplicate: DuplicatePolicy,
    cascade: CascadePolicy,
}

impl DirectiveContractTemplate {
    pub(crate) fn freeze(self) -> DirectiveContract {
        DirectiveContract {
            context: self.context,
            name: self.name,
            value_type: Some((self.value_type)()),
            value_type_name: Some((self.value_type_name)()),
            cardinality: self.cardinality,
            shape: self.shape,
            duplicate: self.duplicate,
            cascade: self.cascade,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectiveContract {
    context: ContextKey,
    name: DirectiveName,
    value_type: Option<TypeId>,
    value_type_name: Option<&'static str>,
    cardinality: DirectiveCardinality,
    shape: DirectiveShape,
    duplicate: DuplicatePolicy,
    cascade: CascadePolicy,
}

fn value_type_name<T>() -> &'static str {
    std::any::type_name::<T>()
}

impl DirectiveContract {
    pub(crate) const fn context(self) -> ContextKey {
        self.context
    }

    pub(crate) const fn name(self) -> DirectiveName {
        self.name
    }

    pub(crate) const fn cascade(self) -> CascadePolicy {
        self.cascade
    }

    pub(crate) fn mismatch(self, actual: Self) -> Option<DirectiveContractMismatch> {
        if self.name != actual.name {
            return Some(DirectiveContractMismatch::Name {
                expected: self.name,
                actual: Some(actual.name),
            });
        }
        if self.context != actual.context {
            return Some(DirectiveContractMismatch::Context {
                expected: self.context,
                actual: actual.context,
            });
        }
        if self.cardinality != actual.cardinality {
            return Some(DirectiveContractMismatch::Cardinality {
                expected: self.cardinality,
                actual: actual.cardinality,
            });
        }
        if let (
            DirectiveShape::ContextBlock {
                payload: expected, ..
            },
            DirectiveShape::ContextBlock {
                payload: actual, ..
            },
        ) = (self.shape, actual.shape)
            && expected != actual
        {
            return Some(DirectiveContractMismatch::Payload { expected, actual });
        }
        if self.value_type != actual.value_type {
            return Some(DirectiveContractMismatch::ValueType {
                expected: self.value_type_name.unwrap_or("<none>"),
                actual: actual.value_type_name.unwrap_or("<none>"),
            });
        }
        if self.shape != actual.shape {
            return Some(DirectiveContractMismatch::Shape {
                expected: self.shape,
                actual: actual.shape,
            });
        }
        if self.duplicate != actual.duplicate {
            return Some(DirectiveContractMismatch::Duplicate {
                expected: self.duplicate,
                actual: actual.duplicate,
            });
        }
        (self.cascade != actual.cascade).then_some(DirectiveContractMismatch::Cascade {
            expected: self.cascade,
            actual: actual.cascade,
        })
    }
}

#[derive(Debug)]
pub struct LocalDirectiveKey<T> {
    name: DirectiveName,
    contract: DirectiveContractTemplate,
    value: std::marker::PhantomData<fn() -> T>,
}

impl<T> Clone for LocalDirectiveKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for LocalDirectiveKey<T> {}

impl<T> LocalDirectiveKey<T> {
    const fn new(name: DirectiveName, contract: DirectiveContractTemplate) -> Self {
        Self {
            name,
            contract,
            value: std::marker::PhantomData,
        }
    }

    pub const fn name(self) -> DirectiveName {
        self.name
    }

    pub(crate) fn contract(self) -> DirectiveContract {
        self.contract.freeze()
    }
}

#[derive(Debug)]
pub struct RepeatedDirectiveKey<T> {
    name: DirectiveName,
    contract: DirectiveContractTemplate,
    value: std::marker::PhantomData<fn() -> T>,
}

impl<T> Clone for RepeatedDirectiveKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for RepeatedDirectiveKey<T> {}

impl<T> RepeatedDirectiveKey<T> {
    const fn new(name: DirectiveName, contract: DirectiveContractTemplate) -> Self {
        Self {
            name,
            contract,
            value: std::marker::PhantomData,
        }
    }

    pub const fn name(self) -> DirectiveName {
        self.name
    }

    pub(crate) fn contract(self) -> DirectiveContract {
        self.contract.freeze()
    }
}

#[derive(Debug)]
pub struct ContextPayloadKey<T> {
    name: DirectiveName,
    contract: DirectiveContractTemplate,
    value: std::marker::PhantomData<fn() -> T>,
}

impl<T> Clone for ContextPayloadKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ContextPayloadKey<T> {}

impl<T> ContextPayloadKey<T> {
    const fn new(name: DirectiveName, contract: DirectiveContractTemplate) -> Self {
        Self {
            name,
            contract,
            value: std::marker::PhantomData,
        }
    }

    pub const fn name(self) -> DirectiveName {
        self.name
    }

    pub(crate) fn contract(self) -> DirectiveContract {
        self.contract.freeze()
    }
}

type DirectiveParserFn =
    fn(&DirectiveInput<'_>) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>>;
pub type ContextFinalizeFn =
    fn(&mut ConfigNode, &BuildOptions<'_>) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
type AttachedContextFinalizeFn =
    for<'tree> fn(
        AttachedConfigNode<'tree>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub trait DirectiveValue: ConfigValue + Sized {
    type Error: Error + Send + Sync + 'static;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        input.directive.span
    }
}

pub struct DirectiveInput<'a> {
    pub directive: &'a AstDirective,
    pub context: ContextKey,
    pub source_map: &'a SourceMap,
}

pub enum ParsedDirective {
    Slot(TypedValue),
    Payload(TypedValue),
    Empty,
}

impl DirectiveSpec {
    pub fn leaf_value<T>(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name: DirectiveName::new(name),
            allowed_in,
            shape: DirectiveShape::Leaf,
            parser: slot_value_parser::<T>,
            duplicate,
            cascade,
            transport,
            reload,
            value_type: Some(TypeId::of::<T>()),
            value_type_name: Some(std::any::type_name::<T>()),
        }
    }

    pub fn raw_value<T>(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name: DirectiveName::new(name),
            allowed_in,
            shape: DirectiveShape::RawBlock,
            parser: slot_value_parser::<T>,
            duplicate,
            cascade,
            transport,
            reload,
            value_type: Some(TypeId::of::<T>()),
            value_type_name: Some(std::any::type_name::<T>()),
        }
    }

    pub fn context_empty(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        child_context: ContextKey,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self {
            name: DirectiveName::new(name),
            allowed_in,
            shape: DirectiveShape::ContextBlock {
                child_context,
                payload: PayloadMode::None,
            },
            parser: empty_parser,
            duplicate,
            cascade,
            transport,
            reload,
            value_type: None,
            value_type_name: None,
        }
    }

    pub fn context_payload<T>(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        child_context: ContextKey,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name: DirectiveName::new(name),
            allowed_in,
            shape: DirectiveShape::ContextBlock {
                child_context,
                payload: PayloadMode::Parser,
            },
            parser: payload_value_parser::<T>,
            duplicate,
            cascade,
            transport,
            reload,
            value_type: Some(TypeId::of::<T>()),
            value_type_name: Some(std::any::type_name::<T>()),
        }
    }

    fn contract(&self, registration_context: ContextKey) -> DirectiveContract {
        let (context, cardinality) = match self.shape {
            DirectiveShape::Leaf | DirectiveShape::RawBlock => (
                registration_context,
                if self.duplicate == DuplicatePolicy::Append {
                    DirectiveCardinality::Repeated
                } else {
                    DirectiveCardinality::Single
                },
            ),
            DirectiveShape::ContextBlock { child_context, .. } => {
                (child_context, DirectiveCardinality::ContextPayload)
            }
        };
        DirectiveContract {
            context,
            name: self.name,
            value_type: self.value_type,
            value_type_name: self.value_type_name,
            cardinality,
            shape: self.shape,
            duplicate: self.duplicate,
            cascade: self.cascade,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SingleCardinality;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RepeatedCardinality;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PayloadCardinality;

#[derive(Debug, Clone, Copy)]
pub(crate) struct CascadedCardinality;

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectiveMetadata {
    duplicate: DuplicatePolicy,
    cascade: CascadePolicy,
    transport: TransportPolicy,
    reload: ReloadImpact,
}

impl DirectiveMetadata {
    pub(crate) const fn new(
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self {
            duplicate,
            cascade,
            transport,
            reload,
        }
    }
}

#[derive(Debug)]
pub(crate) struct DirectiveProjection<T> {
    builtin: Option<BuiltinValue<T>>,
    snapshot: Option<SnapshotValue<T>>,
}

impl<T> Clone for DirectiveProjection<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for DirectiveProjection<T> {}

impl<T> DirectiveProjection<T> {
    pub(crate) const fn new(builtin: BuiltinValue<T>, snapshot: SnapshotValue<T>) -> Self {
        Self {
            builtin: Some(builtin),
            snapshot: Some(snapshot),
        }
    }

    pub(crate) const fn absent() -> Self {
        Self {
            builtin: None,
            snapshot: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct TypedDirectiveDefinition<T, Cardinality> {
    registration_context: ContextKey,
    contract_context: ContextKey,
    name: DirectiveName,
    shape: DirectiveShape,
    cardinality: DirectiveCardinality,
    duplicate: DuplicatePolicy,
    cascade: CascadePolicy,
    transport: TransportPolicy,
    reload: ReloadImpact,
    builtin: Option<BuiltinValue<T>>,
    snapshot: Option<SnapshotValue<T>>,
    value: std::marker::PhantomData<fn() -> Cardinality>,
}

impl<T, Cardinality> Clone for TypedDirectiveDefinition<T, Cardinality> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, Cardinality> Copy for TypedDirectiveDefinition<T, Cardinality> {}

impl<T, Cardinality> TypedDirectiveDefinition<T, Cardinality> {
    const fn new(
        registration_context: ContextKey,
        contract_context: ContextKey,
        name: &'static str,
        shape: DirectiveShape,
        cardinality: DirectiveCardinality,
        metadata: DirectiveMetadata,
        projection: DirectiveProjection<T>,
    ) -> Self {
        Self {
            registration_context,
            contract_context,
            name: DirectiveName::new(name),
            shape,
            cardinality,
            duplicate: metadata.duplicate,
            cascade: metadata.cascade,
            transport: metadata.transport,
            reload: metadata.reload,
            builtin: projection.builtin,
            snapshot: projection.snapshot,
            value: std::marker::PhantomData,
        }
    }

    const fn contract_template(self) -> DirectiveContractTemplate
    where
        T: 'static,
    {
        DirectiveContractTemplate {
            context: self.contract_context,
            name: self.name,
            value_type: TypeId::of::<T>,
            value_type_name: value_type_name::<T>,
            cardinality: self.cardinality,
            shape: self.shape,
            duplicate: self.duplicate,
            cascade: self.cascade,
        }
    }

    pub(crate) fn register(self, registry: &mut ConfigRegistry)
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        let spec = match self.shape {
            DirectiveShape::Leaf => DirectiveSpec::leaf_value::<T>(
                self.name.as_str(),
                vec![self.registration_context],
                self.duplicate,
                self.cascade,
                self.transport,
                self.reload,
            ),
            DirectiveShape::RawBlock => DirectiveSpec::raw_value::<T>(
                self.name.as_str(),
                vec![self.registration_context],
                self.duplicate,
                self.cascade,
                self.transport,
                self.reload,
            ),
            DirectiveShape::ContextBlock { child_context, .. } => {
                DirectiveSpec::context_payload::<T>(
                    self.name.as_str(),
                    vec![self.registration_context],
                    child_context,
                    self.duplicate,
                    self.cascade,
                    self.transport,
                    self.reload,
                )
            }
        };
        registry.register_directive(self.registration_context, spec);
    }
}

impl<T> TypedDirectiveDefinition<T, SingleCardinality> {
    pub(crate) const fn single_leaf(
        context: ContextKey,
        name: &'static str,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self::new(
            context,
            context,
            name,
            DirectiveShape::Leaf,
            DirectiveCardinality::Single,
            DirectiveMetadata::new(duplicate, cascade, transport, reload),
            DirectiveProjection::absent(),
        )
    }

    pub(crate) const fn key(self) -> LocalDirectiveKey<T>
    where
        T: 'static,
    {
        LocalDirectiveKey::new(self.name, self.contract_template())
    }
}

impl<T> TypedDirectiveDefinition<T, RepeatedCardinality> {
    pub(crate) const fn repeated_raw(
        context: ContextKey,
        name: &'static str,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self::new(
            context,
            context,
            name,
            DirectiveShape::RawBlock,
            DirectiveCardinality::Repeated,
            DirectiveMetadata::new(DuplicatePolicy::Append, cascade, transport, reload),
            DirectiveProjection::absent(),
        )
    }

    pub(crate) const fn repeated_leaf(
        context: ContextKey,
        name: &'static str,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self::new(
            context,
            context,
            name,
            DirectiveShape::Leaf,
            DirectiveCardinality::Repeated,
            DirectiveMetadata::new(DuplicatePolicy::Append, cascade, transport, reload),
            DirectiveProjection::absent(),
        )
    }

    pub(crate) const fn key(self) -> RepeatedDirectiveKey<T>
    where
        T: 'static,
    {
        RepeatedDirectiveKey::new(self.name, self.contract_template())
    }
}

impl<T> TypedDirectiveDefinition<T, PayloadCardinality> {
    pub(crate) const fn payload(
        registration_context: ContextKey,
        child_context: ContextKey,
        name: &'static str,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self::new(
            registration_context,
            child_context,
            name,
            DirectiveShape::ContextBlock {
                child_context,
                payload: PayloadMode::Parser,
            },
            DirectiveCardinality::ContextPayload,
            DirectiveMetadata::new(duplicate, cascade, transport, reload),
            DirectiveProjection::absent(),
        )
    }

    pub(crate) const fn key(self) -> ContextPayloadKey<T>
    where
        T: 'static,
    {
        ContextPayloadKey::new(self.name, self.contract_template())
    }
}

impl<T> TypedDirectiveDefinition<T, CascadedCardinality> {
    pub(crate) const fn cascaded(
        context: ContextKey,
        name: &'static str,
        shape: DirectiveShape,
        metadata: DirectiveMetadata,
        projection: DirectiveProjection<T>,
    ) -> Self {
        Self::new(
            context,
            context,
            name,
            shape,
            DirectiveCardinality::Single,
            metadata,
            projection,
        )
    }

    pub(crate) const fn cascaded_leaf(
        context: ContextKey,
        name: &'static str,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self::cascaded(
            context,
            name,
            DirectiveShape::Leaf,
            DirectiveMetadata::new(duplicate, cascade, transport, reload),
            DirectiveProjection::absent(),
        )
    }

    pub(crate) const fn cascaded_raw(
        context: ContextKey,
        name: &'static str,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self::cascaded(
            context,
            name,
            DirectiveShape::RawBlock,
            DirectiveMetadata::new(duplicate, cascade, transport, reload),
            DirectiveProjection::absent(),
        )
    }

    pub(crate) const fn key(self) -> DirectiveKey<T>
    where
        T: 'static,
    {
        let builtin = match self.builtin {
            Some(builtin) => builtin,
            None => crate::parse::cascade::absent,
        };
        let snapshot = match self.snapshot {
            Some(snapshot) => snapshot,
            None => crate::parse::cascade::no_snapshot,
        };
        DirectiveKey::new(self.name, self.contract_template(), builtin, snapshot)
    }

    pub(crate) const fn key_inheriting(self, parent: DirectiveKey<T>) -> DirectiveKey<T>
    where
        T: 'static,
    {
        parent.inheriting(self.contract_template())
    }
}

#[derive(Debug, Clone, Copy)]
struct ErasedV1SnapshotDirective {
    name: DirectiveName,
    shape: DirectiveShape,
    duplicate: DuplicatePolicy,
    cascade: CascadePolicy,
    transport: TransportPolicy,
    reload: ReloadImpact,
    value_type: fn() -> TypeId,
    value_type_name: &'static str,
}

impl ErasedV1SnapshotDirective {
    fn from_definition<T>(definition: TypedDirectiveDefinition<T, CascadedCardinality>) -> Self
    where
        T: 'static,
    {
        Self {
            name: definition.name,
            shape: definition.shape,
            duplicate: definition.duplicate,
            cascade: definition.cascade,
            transport: definition.transport,
            reload: definition.reload,
            value_type: TypeId::of::<T>,
            value_type_name: std::any::type_name::<T>(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReservedV1SnapshotField {
    name: DirectiveName,
}

impl ReservedV1SnapshotField {
    const fn new(name: &'static str) -> Self {
        Self {
            name: DirectiveName::new(name),
        }
    }

    pub(crate) const fn name(self) -> DirectiveName {
        self.name
    }
}

macro_rules! define_v1_snapshot_schema {
    (
        $(
            $field:ident: $value_type:ty =
                $name:literal, $shape:expr, $duplicate:expr, $cascade:expr,
                $transport:expr, $reload:expr, $builtin:expr, $snapshot:expr;
        )+
        @reserved $reserved:ident = $reserved_name:literal;
    ) => {
        #[derive(Debug)]
        pub(crate) struct V1SnapshotSchema {
            $(pub(crate) $field: TypedDirectiveDefinition<$value_type, CascadedCardinality>,)+
            pub(crate) $reserved: ReservedV1SnapshotField,
        }

        impl V1SnapshotSchema {
            fn active_directives(
                &self,
            ) -> impl Iterator<Item = ErasedV1SnapshotDirective> + '_ {
                [$(ErasedV1SnapshotDirective::from_definition(self.$field),)+].into_iter()
            }

            fn register_active_directives(&self, registry: &mut ConfigRegistry) {
                $(self.$field.register(registry);)+
            }

            #[cfg(test)]
            pub(crate) const fn field_names(&self) -> [&'static str; 9] {
                [$(self.$field.name.as_str(),)+ self.$reserved.name().as_str()]
            }
        }

        static V1_SNAPSHOT_SCHEMA: V1SnapshotSchema = V1SnapshotSchema {
            $($field: TypedDirectiveDefinition::cascaded(
                context::PISHOO,
                $name,
                $shape,
                DirectiveMetadata::new($duplicate, $cascade, $transport, $reload),
                DirectiveProjection::new($builtin, $snapshot),
            ),)+
            $reserved: ReservedV1SnapshotField::new($reserved_name),
        };
    };
}

define_v1_snapshot_schema! {
    access_rules: AccessRulesUri = "access_rules", DirectiveShape::Leaf, DuplicatePolicy::Reject,
        CascadePolicy::NearestWins, TransportPolicy::WorkerInheritable,
        ReloadImpact::RuntimeState, crate::parse::cascade::absent,
        RootConfigSnapshot::cascade_access_rules;
    gzip: BoolConfig = "gzip", DirectiveShape::Leaf, DuplicatePolicy::Reject,
        CascadePolicy::NearestWins, TransportPolicy::WorkerInheritable,
        ReloadImpact::RuntimeState, crate::parse::cascade::builtin_false,
        RootConfigSnapshot::cascade_gzip;
    gzip_vary: BoolConfig = "gzip_vary", DirectiveShape::Leaf, DuplicatePolicy::Reject,
        CascadePolicy::NearestWins, TransportPolicy::WorkerInheritable,
        ReloadImpact::RuntimeState, crate::parse::cascade::builtin_false,
        RootConfigSnapshot::cascade_gzip_vary;
    gzip_min_length: GzipMinLength = "gzip_min_length", DirectiveShape::Leaf,
        DuplicatePolicy::Reject,
        CascadePolicy::NearestWins, TransportPolicy::WorkerInheritable,
        ReloadImpact::RuntimeState, crate::parse::cascade::builtin_min_length,
        RootConfigSnapshot::cascade_gzip_min_length;
    gzip_comp_level: GzipCompLevel = "gzip_comp_level", DirectiveShape::Leaf,
        DuplicatePolicy::Reject,
        CascadePolicy::NearestWins, TransportPolicy::WorkerInheritable,
        ReloadImpact::RuntimeState, crate::parse::cascade::builtin_comp_level,
        RootConfigSnapshot::cascade_gzip_comp_level;
    gzip_types: StringList = "gzip_types", DirectiveShape::Leaf, DuplicatePolicy::Reject,
        CascadePolicy::NearestWins, TransportPolicy::WorkerInheritable,
        ReloadImpact::RuntimeState, crate::parse::cascade::absent,
        RootConfigSnapshot::cascade_gzip_types;
    default_type: DefaultType = "default_type", DirectiveShape::Leaf, DuplicatePolicy::Reject,
        CascadePolicy::NearestWins, TransportPolicy::WorkerInheritable,
        ReloadImpact::RuntimeState, crate::parse::cascade::absent,
        RootConfigSnapshot::cascade_default_type;
    types: MimeTypes = "types", DirectiveShape::RawBlock, DuplicatePolicy::Reject,
        CascadePolicy::ReplaceWhole, TransportPolicy::WorkerInheritable,
        ReloadImpact::RuntimeState, crate::parse::cascade::absent,
        RootConfigSnapshot::cascade_types;
    @reserved access_log = "access_log";
}

#[cfg(test)]
pub(crate) const fn v1_snapshot_field_names() -> [&'static str; 9] {
    V1_SNAPSHOT_SCHEMA.field_names()
}

pub(crate) const fn v1_gzip_key() -> DirectiveKey<BoolConfig> {
    V1_SNAPSHOT_SCHEMA.gzip.key()
}

pub(crate) const fn v1_gzip_types_key() -> DirectiveKey<StringList> {
    V1_SNAPSHOT_SCHEMA.gzip_types.key()
}

pub(crate) const fn v1_access_rules_key() -> DirectiveKey<AccessRulesUri> {
    V1_SNAPSHOT_SCHEMA.access_rules.key()
}

pub(crate) const fn v1_gzip_vary_key() -> DirectiveKey<BoolConfig> {
    V1_SNAPSHOT_SCHEMA.gzip_vary.key()
}

pub(crate) const fn v1_gzip_min_length_key() -> DirectiveKey<GzipMinLength> {
    V1_SNAPSHOT_SCHEMA.gzip_min_length.key()
}

pub(crate) const fn v1_gzip_comp_level_key() -> DirectiveKey<GzipCompLevel> {
    V1_SNAPSHOT_SCHEMA.gzip_comp_level.key()
}

pub(crate) const fn v1_default_type_key() -> DirectiveKey<DefaultType> {
    V1_SNAPSHOT_SCHEMA.default_type.key()
}

pub(crate) const fn v1_types_key() -> DirectiveKey<MimeTypes> {
    V1_SNAPSHOT_SCHEMA.types.key()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ValidatedV1SnapshotSchema {
    schema: &'static V1SnapshotSchema,
}

impl ValidatedV1SnapshotSchema {
    pub(crate) const fn schema(self) -> &'static V1SnapshotSchema {
        self.schema
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum V1SnapshotSchemaError {
    #[snafu(display("root snapshot directive `{directive}` is not registered in PISHOO"))]
    MissingDirective { directive: DirectiveName },
    #[snafu(display(
        "root snapshot directive `{directive}` has value type `{actual}`, expected `{expected}`"
    ))]
    ValueType {
        directive: DirectiveName,
        expected: &'static str,
        actual: &'static str,
    },
    #[snafu(display("root snapshot directive `{directive}` has an incompatible shape"))]
    Shape { directive: DirectiveName },
    #[snafu(display("root snapshot directive `{directive}` has an incompatible duplicate policy"))]
    Duplicate { directive: DirectiveName },
    #[snafu(display("root snapshot directive `{directive}` has an incompatible cascade policy"))]
    Cascade { directive: DirectiveName },
    #[snafu(display(
        "root snapshot directive `{directive}` is not registered as WorkerInheritable"
    ))]
    Transport { directive: DirectiveName },
    #[snafu(display("root snapshot directive `{directive}` has an incompatible reload impact"))]
    Reload { directive: DirectiveName },
    #[snafu(display(
        "worker-inheritable PISHOO directive `{directive}` is not part of the V1 snapshot schema"
    ))]
    ExtraWorkerInheritableDirective { directive: DirectiveName },
    #[snafu(display(
        "reserved root snapshot directive `{directive}` was registered before its checked domain"
    ))]
    PrematureReservedDirective { directive: DirectiveName },
}

fn slot_value_parser<T>(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>>
where
    T: DirectiveValue,
    for<'input, 'directive> T:
        TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
{
    let span = T::span(input);
    match T::try_from(input) {
        Ok(value) => Ok(ParsedDirective::Slot(TypedValue::new(value, span))),
        Err(source) => Err(Box::new(source)),
    }
}

fn payload_value_parser<T>(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>>
where
    T: DirectiveValue,
    for<'input, 'directive> T:
        TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
{
    let span = T::span(input);
    match T::try_from(input) {
        Ok(value) => Ok(ParsedDirective::Payload(TypedValue::new(value, span))),
        Err(source) => Err(Box::new(source)),
    }
}

fn empty_parser(
    _input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    Ok(ParsedDirective::Empty)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BuildOptions<'a> {
    pub dhttp_home: Option<&'a dhttp::home::DhttpHome>,
    pub identity_profile: Option<&'a dhttp::home::identity::IdentityProfile>,
}

impl BuildOptions<'_> {
    pub fn has_dhttp_home_context(&self) -> bool {
        self.dhttp_home.is_some() || self.identity_profile.is_some()
    }
}

impl ConfigRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_context(&mut self, spec: ContextSpec) {
        self.contexts.insert(spec.key, spec);
        self.advance_contract();
    }

    pub fn register_directive(&mut self, context: ContextKey, spec: DirectiveSpec) {
        self.directives.insert((context, spec.name.as_str()), spec);
        self.rebuild_contract_tables();
        self.advance_contract();
    }

    fn rebuild_contract_tables(&mut self) {
        let mut contract_tables = HashMap::<ContextKey, Vec<DirectiveContract>>::new();
        for ((registration_context, _), spec) in &self.directives {
            let contract = spec.contract(*registration_context);
            contract_tables
                .entry(contract.context())
                .or_default()
                .push(contract);
        }
        self.contract_tables = contract_tables
            .into_iter()
            .map(|(context, mut contracts)| {
                contracts.sort_unstable_by_key(|contract| {
                    (contract.name.as_str(), contract.cardinality as u8)
                });
                contracts.dedup();
                (context, Arc::from(contracts.into_boxed_slice()))
            })
            .collect();
    }

    pub(crate) fn register_attached_finalizer(
        &mut self,
        context: ContextKey,
        finalize: AttachedContextFinalizeFn,
    ) {
        self.attached_finalizers.insert(context, finalize);
        self.advance_contract();
    }

    fn advance_contract(&mut self) {
        self.contract = ConfigRegistryContract::default();
    }

    pub(crate) fn contract(&self) -> ConfigRegistryContract {
        self.contract.clone()
    }

    pub(crate) fn matches_contract(&self, contract: &ConfigRegistryContract) -> bool {
        self.contract.matches(contract)
    }

    pub(crate) fn finalize_attached(
        &self,
        node: AttachedConfigNode<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(finalize) = self.attached_finalizers.get(&node.context()) {
            finalize(node)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn directive_spec(&self, context: ContextKey, name: &str) -> Option<&DirectiveSpec> {
        self.directives
            .iter()
            .find_map(|((registered_context, registered_name), spec)| {
                (*registered_context == context && *registered_name == name).then_some(spec)
            })
    }

    pub(crate) fn frozen_contract_tables(&self) -> HashMap<ContextKey, Arc<[DirectiveContract]>> {
        self.contract_tables.clone()
    }

    pub(crate) fn register_v1_snapshot_directives(&mut self) {
        V1_SNAPSHOT_SCHEMA.register_active_directives(self);
    }

    pub(crate) fn validate_v1_snapshot_schema(
        &self,
    ) -> Result<ValidatedV1SnapshotSchema, V1SnapshotSchemaError> {
        for ((registered_context, registered_name), spec) in &self.directives {
            if *registered_context != context::PISHOO {
                continue;
            }
            let directive = DirectiveName::new(registered_name);
            if *registered_name == V1_SNAPSHOT_SCHEMA.access_log.name().as_str() {
                return Err(V1SnapshotSchemaError::PrematureReservedDirective { directive });
            }
            if spec.transport == TransportPolicy::WorkerInheritable
                && !V1_SNAPSHOT_SCHEMA
                    .active_directives()
                    .any(|expected| expected.name.as_str() == *registered_name)
            {
                return Err(V1SnapshotSchemaError::ExtraWorkerInheritableDirective { directive });
            }
        }

        for expected in V1_SNAPSHOT_SCHEMA.active_directives() {
            let spec = self
                .directives
                .get(&(context::PISHOO, expected.name.as_str()))
                .ok_or(V1SnapshotSchemaError::MissingDirective {
                    directive: expected.name,
                })?;
            if spec.value_type != Some((expected.value_type)()) {
                return Err(V1SnapshotSchemaError::ValueType {
                    directive: expected.name,
                    expected: expected.value_type_name,
                    actual: spec.value_type_name.unwrap_or("<none>"),
                });
            }
            if spec.shape != expected.shape {
                return Err(V1SnapshotSchemaError::Shape {
                    directive: expected.name,
                });
            }
            if spec.duplicate != expected.duplicate {
                return Err(V1SnapshotSchemaError::Duplicate {
                    directive: expected.name,
                });
            }
            if spec.cascade != expected.cascade {
                return Err(V1SnapshotSchemaError::Cascade {
                    directive: expected.name,
                });
            }
            if spec.transport != expected.transport {
                return Err(V1SnapshotSchemaError::Transport {
                    directive: expected.name,
                });
            }
            if spec.reload != expected.reload {
                return Err(V1SnapshotSchemaError::Reload {
                    directive: expected.name,
                });
            }
        }

        Ok(ValidatedV1SnapshotSchema {
            schema: &V1_SNAPSHOT_SCHEMA,
        })
    }

    pub fn build(
        &self,
        source_map: Arc<SourceMap>,
        directives: Vec<AstDirective>,
        options: BuildOptions<'_>,
    ) -> Result<ConfigDocument, BuildDocumentError> {
        let span = directives
            .first()
            .map(|directive| directive.span)
            .unwrap_or(SourceSpan {
                source_id: crate::parse::source::SourceId(0),
                start: 0,
                end: 0,
            });
        let mut root = ConfigNode::new(context::ROOT, None, span);
        self.build_into(
            source_map.as_ref(),
            &mut root,
            context::ROOT,
            directives,
            &options,
        )?;
        let root = Arc::new(root);
        put_parent_recursively(&root, None)?;
        Ok(ConfigDocument::new(source_map, root))
    }

    pub(crate) fn build_for_role(
        &self,
        sources: Arc<ConfigDocumentSourceMap>,
        directives: Vec<AstDirective>,
        options: BuildOptions<'_>,
        role: ConfigDocumentRoleKind,
    ) -> Result<ParsedConfigDocument, RoleDocumentBuildError> {
        let registry_contract = self.contract();
        self.validate_role_registration(&sources, &directives, role)?;
        validate_document_shape(&sources, &directives, role)?;
        self.validate_role_directives(&sources, &directives, context::ROOT, role)?;

        let span = document_span(&directives);
        let mut root = ConfigNode::new(context::ROOT, None, span);
        self.build_into(
            sources.source_map(),
            &mut root,
            context::ROOT,
            directives,
            &options,
        )
        .map_err(RoleDocumentBuildError::Build)?;

        match role {
            ConfigDocumentRoleKind::HypervisorRoot => {
                let pishoo = self.required_built_child(&sources, &root, "pishoo", role)?;
                Ok(ParsedConfigDocument::HypervisorRoot(
                    ParsedPishooFragment::new(sources, pishoo, registry_contract),
                ))
            }
            ConfigDocumentRoleKind::WorkerPishoo => {
                let pishoo = self.required_built_child(&sources, &root, "pishoo", role)?;
                Ok(ParsedConfigDocument::WorkerPishoo(
                    ParsedPishooFragment::new(sources, pishoo, registry_contract),
                ))
            }
            ConfigDocumentRoleKind::IdentityServer => {
                let servers: Box<[_]> = root
                    .children_optional("server")
                    .iter()
                    .cloned()
                    .map(|server| {
                        ParsedServerFragment::new(
                            Arc::clone(&sources),
                            server,
                            registry_contract.clone(),
                        )
                    })
                    .collect();
                if servers.is_empty() {
                    return Err(RoleDocumentBuildError::Role(
                        ConfigDocumentRoleError::missing_built_directive(
                            DirectiveName::new("server"),
                            role,
                            sources.config_span(root.span),
                        ),
                    ));
                }
                Ok(ParsedConfigDocument::IdentityServers(servers))
            }
        }
    }

    fn validate_role_registration(
        &self,
        sources: &ConfigDocumentSourceMap,
        directives: &[AstDirective],
        role: ConfigDocumentRoleKind,
    ) -> Result<(), RoleDocumentBuildError> {
        let (directive, expected_child_context) = match role {
            ConfigDocumentRoleKind::HypervisorRoot | ConfigDocumentRoleKind::WorkerPishoo => {
                (DirectiveName::new("pishoo"), context::PISHOO)
            }
            ConfigDocumentRoleKind::IdentityServer => {
                (DirectiveName::new("server"), context::SERVER)
            }
        };
        let spec = self.directives.get(&(context::ROOT, directive.as_str()));
        let valid = spec.is_some_and(|spec| {
            spec.allowed_in.contains(&context::ROOT)
                && matches!(
                    spec.shape,
                    DirectiveShape::ContextBlock {
                        child_context,
                        payload: PayloadMode::None,
                    } if child_context == expected_child_context
                )
                && self.contexts.contains_key(&expected_child_context)
        });
        if valid {
            return Ok(());
        }
        let span = directives
            .iter()
            .find(|candidate| candidate.name.value == directive.as_str())
            .map_or_else(
                || document_span(directives),
                |candidate| candidate.name.span,
            );
        Err(RoleDocumentBuildError::Role(
            ConfigDocumentRoleError::invalid_directive_registration(
                directive,
                role,
                expected_child_context,
                sources.config_span(span),
            ),
        ))
    }

    fn required_built_child(
        &self,
        sources: &ConfigDocumentSourceMap,
        root: &ConfigNode,
        directive: &'static str,
        role: ConfigDocumentRoleKind,
    ) -> Result<Arc<ConfigNode>, RoleDocumentBuildError> {
        root.children_optional(directive)
            .first()
            .cloned()
            .ok_or_else(|| {
                RoleDocumentBuildError::Role(ConfigDocumentRoleError::missing_built_directive(
                    DirectiveName::new(directive),
                    role,
                    sources.config_span(root.span),
                ))
            })
    }

    fn validate_role_directives(
        &self,
        sources: &ConfigDocumentSourceMap,
        directives: &[AstDirective],
        context: ContextKey,
        role: ConfigDocumentRoleKind,
    ) -> Result<(), RoleDocumentBuildError> {
        for directive in directives {
            let Some(spec) = self
                .directives
                .get(&(context, directive.name.value.as_str()))
            else {
                continue;
            };
            if !directive_allowed_for_role(spec, context, role) {
                return Err(RoleDocumentBuildError::Role(
                    ConfigDocumentRoleError::directive_not_allowed(
                        spec.name,
                        role,
                        sources.config_span(directive.name.span),
                    ),
                ));
            }
            if let (
                DirectiveShape::ContextBlock { child_context, .. },
                AstBody::Block { children, .. },
            ) = (spec.shape, &directive.body)
            {
                self.validate_role_directives(sources, children, child_context, role)?;
            }
        }
        Ok(())
    }

    fn build_into(
        &self,
        source_map: &SourceMap,
        node: &mut ConfigNode,
        context: ContextKey,
        directives: Vec<AstDirective>,
        options: &BuildOptions<'_>,
    ) -> Result<(), BuildDocumentError> {
        for directive in directives {
            let directive_name = directive.name.value.clone();
            let spec = self
                .directives
                .get(&(context, directive_name.as_str()))
                .with_context(|| build_document_error::UnknownDirectiveSnafu {
                    directive: directive_name.clone(),
                    span: directive.name.span,
                })?;
            apply_shape(&directive, spec)?;
            let parsed = (spec.parser)(&DirectiveInput {
                directive: &directive,
                context,
                source_map,
            })
            .context(build_document_error::DirectiveParseSnafu {
                directive: directive_name.clone(),
                span: directive.span,
            })?;
            match spec.shape {
                DirectiveShape::Leaf | DirectiveShape::RawBlock => {
                    if let ParsedDirective::Slot(value) = parsed {
                        insert_slot(node, spec, value, source_map)?;
                    }
                }
                DirectiveShape::ContextBlock {
                    child_context,
                    payload,
                } => {
                    let mut child = ConfigNode::new(
                        child_context,
                        Some(directive.name.clone()),
                        directive.span,
                    );
                    if matches!(payload, PayloadMode::Parser)
                        && let ParsedDirective::Payload(value) = parsed
                    {
                        child.set_payload(value);
                    }
                    let children = match directive.body {
                        AstBody::Block { children, .. } => children,
                        AstBody::Leaf { .. } => {
                            return build_document_error::InvalidDirectiveShapeSnafu {
                                directive: directive_name,
                                span: directive.span,
                            }
                            .fail();
                        }
                    };
                    self.build_into(source_map, &mut child, child_context, children, options)?;
                    node.insert_child(spec.name.as_str(), Arc::new(child));
                }
            }
        }
        self.finalize_local(node, context, options)?;
        Ok(())
    }

    fn finalize_local(
        &self,
        node: &mut ConfigNode,
        context: ContextKey,
        options: &BuildOptions<'_>,
    ) -> Result<(), BuildDocumentError> {
        if let Some(spec) = self.contexts.get(&context)
            && let Some(finalize) = spec.finalize
        {
            finalize(node, options).context(build_document_error::FinalizeContextSnafu {
                context: context.0,
                span: node.span,
            })?;
        }
        Ok(())
    }
}

fn apply_shape(directive: &AstDirective, spec: &DirectiveSpec) -> Result<(), BuildDocumentError> {
    match (&directive.body, spec.shape) {
        (AstBody::Leaf { .. }, DirectiveShape::Leaf | DirectiveShape::RawBlock) => Ok(()),
        (AstBody::Block { .. }, DirectiveShape::ContextBlock { .. } | DirectiveShape::RawBlock) => {
            Ok(())
        }
        _ => build_document_error::InvalidDirectiveShapeSnafu {
            directive: directive.name.value.clone(),
            span: directive.span,
        }
        .fail(),
    }
}

fn insert_slot(
    node: &mut ConfigNode,
    spec: &DirectiveSpec,
    value: TypedValue,
    source_map: &SourceMap,
) -> Result<(), BuildDocumentError> {
    let span = value.span();
    let value = normalize::normalize_slot_value(value, source_map).context(
        build_document_error::NormalizeDirectiveValueSnafu {
            directive: spec.name.as_str().to_owned(),
            span,
        },
    )?;

    match spec.duplicate {
        DuplicatePolicy::Reject if !node.get_all_untyped(spec.name.as_str()).is_empty() => {
            let first = node.get_all_untyped(spec.name.as_str())[0].span();
            build_document_error::DuplicateDirectiveSnafu {
                directive: spec.name.as_str().to_owned(),
                first,
                duplicate: value.span(),
            }
            .fail()
        }
        DuplicatePolicy::LastWins => {
            node.replace_slot(spec.name.as_str(), value);
            Ok(())
        }
        _ => {
            node.insert_slot(spec.name.as_str(), value);
            Ok(())
        }
    }
}

fn document_span(directives: &[AstDirective]) -> SourceSpan {
    directives
        .first()
        .map(|directive| directive.span)
        .unwrap_or(SourceSpan {
            source_id: crate::parse::source::SourceId(0),
            start: 0,
            end: 0,
        })
}

fn validate_document_shape(
    sources: &ConfigDocumentSourceMap,
    directives: &[AstDirective],
    role: ConfigDocumentRoleKind,
) -> Result<(), RoleDocumentBuildError> {
    match role {
        ConfigDocumentRoleKind::HypervisorRoot | ConfigDocumentRoleKind::WorkerPishoo => {
            let pishoo = directives
                .iter()
                .filter(|directive| directive.name.value == "pishoo")
                .collect::<Vec<_>>();
            if pishoo.len() != 1 {
                let span = pishoo
                    .get(1)
                    .copied()
                    .or_else(|| directives.first())
                    .map_or_else(|| document_span(directives), |directive| directive.span);
                return Err(RoleDocumentBuildError::Role(
                    ConfigDocumentRoleError::expected_single_pishoo(
                        role,
                        pishoo.len(),
                        sources.config_span(span),
                    ),
                ));
            }
        }
        ConfigDocumentRoleKind::IdentityServer => {
            if !directives
                .iter()
                .any(|directive| directive.name.value == "server")
            {
                return Err(RoleDocumentBuildError::Role(
                    ConfigDocumentRoleError::missing_identity_server(
                        role,
                        sources.config_span(document_span(directives)),
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn directive_allowed_for_role(
    spec: &DirectiveSpec,
    context: ContextKey,
    role: ConfigDocumentRoleKind,
) -> bool {
    match role {
        ConfigDocumentRoleKind::HypervisorRoot => {
            context != context::ROOT || spec.name.as_str() == "pishoo"
        }
        ConfigDocumentRoleKind::WorkerPishoo => spec.transport != TransportPolicy::HypervisorOnly,
        ConfigDocumentRoleKind::IdentityServer => {
            context != context::ROOT || spec.name.as_str() == "server"
        }
    }
}

pub(crate) enum RoleDocumentBuildError {
    Role(ConfigDocumentRoleError),
    Build(BuildDocumentError),
}

fn put_parent_recursively(
    node: &Arc<ConfigNode>,
    parent: Option<&Arc<ConfigNode>>,
) -> Result<(), BuildDocumentError> {
    node.set_parent(parent.map(Arc::downgrade))
        .map_err(|span| BuildDocumentError::ParentAlreadyAssigned { span })?;
    for children in node.child_groups() {
        for child in children {
            put_parent_recursively(child, Some(node))?;
        }
    }
    Ok(())
}

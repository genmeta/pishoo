use std::{
    any::{Any, TypeId},
    fmt,
    sync::Arc,
};

use crate::parse::source::SourceSpan;

pub trait ConfigValue: fmt::Debug + Send + Sync + 'static {}

impl<T> ConfigValue for T where T: fmt::Debug + Send + Sync + 'static {}

#[derive(Clone)]
pub struct TypedValue {
    value: Arc<dyn Any + Send + Sync>,
    type_id: TypeId,
    type_name: &'static str,
    span: SourceSpan,
}

impl TypedValue {
    pub fn new<T>(value: T, span: SourceSpan) -> Self
    where
        T: ConfigValue,
    {
        Self {
            value: Arc::new(value),
            type_id: TypeId::of::<T>(),
            type_name: std::any::type_name::<T>(),
            span,
        }
    }

    pub fn downcast<T>(&self) -> Option<Arc<T>>
    where
        T: ConfigValue,
    {
        if self.type_id != TypeId::of::<T>() {
            return None;
        }
        Arc::clone(&self.value).downcast::<T>().ok()
    }

    pub fn type_name(&self) -> &'static str {
        self.type_name
    }

    pub fn span(&self) -> SourceSpan {
        self.span
    }
}

impl fmt::Debug for TypedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypedValue")
            .field("type_name", &self.type_name)
            .field("span", &self.span)
            .finish_non_exhaustive()
    }
}

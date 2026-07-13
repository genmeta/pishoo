use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use tokio::sync::OnceCell;

use crate::parse::{
    ast::Spanned,
    domain::ConfigDocumentId,
    error::{ConfigQueryError, config_query_error},
    registry::ContextKey,
    source::{SourceMap, SourceSpan},
    value::{ConfigValue, TypedValue},
};

#[derive(Debug)]
pub struct ConfigDocument {
    pub source_map: Arc<SourceMap>,
    pub root: Arc<ConfigNode>,
}

#[derive(Debug)]
pub struct ConfigNode {
    pub context: ContextKey,
    pub name: Option<Spanned<String>>,
    pub span: SourceSpan,
    pub payload: Option<TypedValue>,
    slots: HashMap<&'static str, ConfigSlot>,
    children: HashMap<&'static str, Vec<Arc<ConfigNode>>>,
    parent: OnceCell<Option<Weak<ConfigNode>>>,
}

#[derive(Debug, Clone)]
pub struct ConfigSlot {
    pub values: Vec<TypedValue>,
}

impl ConfigDocument {
    pub fn new(source_map: Arc<SourceMap>, root: Arc<ConfigNode>) -> Self {
        Self { source_map, root }
    }

    pub fn document_id(&self) -> ConfigDocumentId {
        self.source_map.document_id()
    }
}

impl ConfigNode {
    pub fn new(context: ContextKey, name: Option<Spanned<String>>, span: SourceSpan) -> Self {
        Self {
            context,
            name,
            span,
            payload: None,
            slots: HashMap::new(),
            children: HashMap::new(),
            parent: OnceCell::new(),
        }
    }

    pub fn insert_slot(&mut self, name: &'static str, value: TypedValue) {
        self.slots
            .entry(name)
            .or_insert_with(|| ConfigSlot { values: Vec::new() })
            .values
            .push(value);
    }

    pub fn replace_slot(&mut self, name: &'static str, value: TypedValue) {
        self.slots.insert(
            name,
            ConfigSlot {
                values: vec![value],
            },
        );
    }

    pub fn get_all_untyped(&self, name: &str) -> &[TypedValue] {
        self.slots
            .get(name)
            .map(|slot| slot.values.as_slice())
            .unwrap_or(&[])
    }

    pub fn insert_child(&mut self, name: &'static str, child: Arc<ConfigNode>) {
        self.children.entry(name).or_default().push(child);
    }

    pub fn child_groups(&self) -> impl Iterator<Item = &[Arc<ConfigNode>]> {
        self.children.values().map(Vec::as_slice)
    }

    pub fn set_payload(&mut self, payload: TypedValue) {
        self.payload = Some(payload);
    }

    pub fn set_parent(&self, parent: Option<Weak<ConfigNode>>) {
        self.parent
            .set(parent)
            .expect("parent link set multiple times for config node");
    }

    pub fn parent(&self) -> Option<Arc<ConfigNode>> {
        self.parent
            .get()
            .and_then(|parent| parent.as_ref())
            .and_then(Weak::upgrade)
    }

    pub fn get<T>(&self, name: &str) -> Result<Option<Arc<T>>, ConfigQueryError>
    where
        T: ConfigValue,
    {
        let Some(slot) = self.slots.get(name) else {
            return Ok(None);
        };
        if slot.values.len() > 1 {
            return config_query_error::MultipleValuesSnafu {
                directive: name.to_owned(),
                span: slot.values[1].span(),
            }
            .fail();
        }
        let value = &slot.values[0];
        value.downcast::<T>().map(Some).ok_or_else(|| {
            config_query_error::TypeMismatchSnafu {
                directive: name.to_owned(),
                expected: std::any::type_name::<T>(),
                actual: value.type_name(),
                span: value.span(),
            }
            .build()
        })
    }

    pub fn require<T>(&self, name: &str) -> Result<Arc<T>, ConfigQueryError>
    where
        T: ConfigValue,
    {
        self.get(name)?.ok_or_else(|| {
            config_query_error::MissingRequiredSnafu {
                directive: name.to_owned(),
                span: self.span,
            }
            .build()
        })
    }

    pub fn get_all<T>(&self, name: &str) -> Result<Vec<Arc<T>>, ConfigQueryError>
    where
        T: ConfigValue,
    {
        let Some(slot) = self.slots.get(name) else {
            return Ok(Vec::new());
        };
        slot.values
            .iter()
            .map(|value| {
                value.downcast::<T>().ok_or_else(|| {
                    config_query_error::TypeMismatchSnafu {
                        directive: name.to_owned(),
                        expected: std::any::type_name::<T>(),
                        actual: value.type_name(),
                        span: value.span(),
                    }
                    .build()
                })
            })
            .collect()
    }

    pub fn inherited<T>(&self, name: &str) -> Result<Option<Arc<T>>, ConfigQueryError>
    where
        T: ConfigValue,
    {
        if let Some(value) = self.get(name)? {
            return Ok(Some(value));
        }
        let Some(parent) = self.parent() else {
            return Ok(None);
        };
        parent.inherited(name)
    }

    pub fn children(&self, name: &str) -> Result<&[Arc<ConfigNode>], ConfigQueryError> {
        self.children.get(name).map(Vec::as_slice).ok_or_else(|| {
            config_query_error::MissingChildSnafu {
                directive: name.to_owned(),
                span: self.span,
            }
            .build()
        })
    }

    pub fn children_optional(&self, name: &str) -> &[Arc<ConfigNode>] {
        self.children.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn payload<T>(&self) -> Result<Option<Arc<T>>, ConfigQueryError>
    where
        T: ConfigValue,
    {
        let Some(payload) = &self.payload else {
            return Ok(None);
        };
        payload.downcast::<T>().map(Some).ok_or_else(|| {
            config_query_error::TypeMismatchSnafu {
                directive: self
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| "<payload>".to_owned()),
                expected: std::any::type_name::<T>(),
                actual: payload.type_name(),
                span: payload.span(),
            }
            .build()
        })
    }
}

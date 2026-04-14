use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Weak},
};

use http::HeaderValue;
use tokio::sync::OnceCell;

use super::Value;

/// Configuration tree node with parent backlinks.
#[derive(Debug)]
pub struct Node {
    value: Value,
    parent: OnceCell<Option<Weak<Node>>>,
}

impl Node {
    pub fn new(value: Value) -> Self {
        assert!(matches!(value, Value::ValueMap(..) | Value::Pattern(..)));

        Self {
            value,
            parent: OnceCell::new(),
        }
    }

    pub fn parent(&self) -> Option<Arc<Node>> {
        self.parent
            .get()
            .and_then(|opt_weak_ref| opt_weak_ref.as_ref())
            .and_then(|weak_parent| weak_parent.upgrade())
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.value.get(key)
    }

    pub(crate) fn set_parent(&self, parent: Option<Weak<Node>>) {
        self.parent.set(parent).expect("parent link set multiple times for the same node. this indicates a bug in the tree transformation logic.");
    }

    pub fn backtrack_node(self: &Arc<Self>, key: &str) -> Option<Arc<Node>> {
        let mut current_node = Arc::clone(self);
        loop {
            if current_node.value().get(key).is_some() {
                return Some(Arc::clone(&current_node));
            }
            let parent = current_node.parent()?;
            current_node = parent;
        }
    }

    pub fn get_value_recursive(self: &Arc<Self>, key: &str) -> Option<Value> {
        self.backtrack_node(key).and_then(|n| n.get(key).cloned())
    }

    pub fn get_bool(self: &Arc<Self>, key: &str) -> Option<bool> {
        match self.get_value_recursive(key) {
            Some(Value::Boolean(b)) => Some(b),
            _ => None,
        }
    }

    pub fn get_str_parsed<T: FromStr>(self: &Arc<Self>, key: &str) -> Option<T> {
        match self.get_value_recursive(key) {
            Some(Value::String(s)) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn get_string_vec(self: &Arc<Self>, key: &str) -> Option<Vec<String>> {
        match self.get_value_recursive(key) {
            Some(Value::StringVec(v)) => Some(v),
            _ => None,
        }
    }

    pub fn get_types(self: &Arc<Self>, key: &str) -> Option<HashMap<String, HeaderValue>> {
        match self.get_value_recursive(key) {
            Some(Value::Types(v)) => Some(v),
            _ => None,
        }
    }

    pub fn get_header_value(self: &Arc<Self>, key: &str) -> Option<HeaderValue> {
        match self.get_value_recursive(key) {
            Some(Value::HeaderValue(v)) => Some(v.clone()),
            _ => None,
        }
    }
}

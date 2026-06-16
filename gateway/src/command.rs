use std::sync::Arc;

use crate::parse::{document::ConfigNode, types::StringList};

pub(crate) mod acl;
pub(crate) mod file;
pub(crate) mod header;
pub(crate) mod variables;

pub use file::IndexError;
pub(crate) use file::index;
pub(crate) use header::{add_header, content_type, proxy_set_header};

pub(crate) fn acl(node: &Arc<ConfigNode>) -> acl::Acl {
    let allow_vec = node
        .get::<StringList>("allow")
        .ok()
        .flatten()
        .map(|allow| allow.0.clone())
        .unwrap_or_default();
    let deny_vec = node
        .get::<StringList>("deny")
        .ok()
        .flatten()
        .map(|deny| deny.0.clone())
        .unwrap_or_default();

    let allow = acl::parse_host_matches(&allow_vec);
    let deny = acl::parse_host_matches(&deny_vec);

    acl::Acl::new(allow, deny)
}

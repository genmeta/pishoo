use std::sync::Arc;

use crate::parse::Node;

pub(crate) mod acl;
pub(crate) mod file;
pub(crate) mod header;
pub(crate) mod variables;

pub use file::IndexError;
pub(crate) use file::index;
pub(crate) use header::{add_header, content_type, proxy_set_header};

pub(crate) fn acl(node: &Arc<Node>) -> acl::Acl {
    let allow_vec = node.get_string_vec("allow").unwrap_or_default();
    let deny_vec = node.get_string_vec("deny").unwrap_or_default();

    let allow = acl::parse_host_matches(&allow_vec);
    let deny = acl::parse_host_matches(&deny_vec);

    acl::Acl::new(allow, deny)
}

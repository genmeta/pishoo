pub(crate) mod acl;
pub(crate) mod file;
pub(crate) mod header;
pub(crate) mod variables;

pub use file::IndexError;
pub(crate) use file::index;
pub(crate) use header::{add_header, content_type, proxy_set_header};

pub(crate) fn acl(allow: &[String], deny: &[String]) -> acl::Acl {
    let allow = acl::parse_host_matches(allow);
    let deny = acl::parse_host_matches(deny);

    acl::Acl::new(allow, deny)
}

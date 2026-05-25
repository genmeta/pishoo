use http::{HeaderName, HeaderValue};
use snafu::{Snafu, ensure};

use crate::parse::{
    builtin::common,
    document::ConfigNode,
    registry::{
        BuildOptions, ConfigRegistry, DirectiveParserFn, DirectiveShape, DirectiveSpec,
        MergePolicy, PayloadMode, context,
    },
    source::SourceSpan,
    types::{HeaderRule, HeaderRules, PathConfig},
    value::TypedValue,
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum FinalizeLocationError {
    #[snafu(display(
        "proxy_ssl_certificate and proxy_ssl_certificate_key must be configured together"
    ))]
    ProxyTlsPair { span: SourceSpan },
}

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::LOCATION,
        finalize: Some(finalize_location),
    });
    registry.register_directive(
        context::SERVER,
        DirectiveSpec {
            name: "location",
            allowed_in: vec![context::SERVER],
            shape: DirectiveShape::ContextBlock {
                child_context: context::LOCATION,
                payload: PayloadMode::Parser,
            },
            parser: common::parse_location_payload,
            merge: MergePolicy::Append,
        },
    );
    for (name, parser, merge) in [
        (
            "root",
            common::parse_path as DirectiveParserFn,
            MergePolicy::RejectDuplicate,
        ),
        ("alias", common::parse_path, MergePolicy::RejectDuplicate),
        ("gzip", common::parse_boolean, MergePolicy::RejectDuplicate),
        (
            "gzip_vary",
            common::parse_boolean,
            MergePolicy::RejectDuplicate,
        ),
        (
            "gzip_min_length",
            common::parse_gzip_min_length,
            MergePolicy::RejectDuplicate,
        ),
        (
            "gzip_comp_level",
            common::parse_gzip_comp_level,
            MergePolicy::RejectDuplicate,
        ),
        (
            "gzip_types",
            common::parse_string_list,
            MergePolicy::RejectDuplicate,
        ),
        (
            "index",
            common::parse_string_list,
            MergePolicy::RejectDuplicate,
        ),
        (
            "add_header",
            common::parse_header_always,
            MergePolicy::Append,
        ),
        (
            "proxy_set_header",
            common::parse_header,
            MergePolicy::Append,
        ),
        (
            "proxy_pass",
            common::parse_proxy_pass,
            MergePolicy::RejectDuplicate,
        ),
        (
            "proxy_ssl_certificate",
            common::parse_path,
            MergePolicy::RejectDuplicate,
        ),
        (
            "proxy_ssl_certificate_key",
            common::parse_path,
            MergePolicy::RejectDuplicate,
        ),
        (
            "proxy_ssl_trusted_certificate",
            common::parse_path,
            MergePolicy::RejectDuplicate,
        ),
        (
            "ssh_login",
            common::parse_ssh_login,
            MergePolicy::RejectDuplicate,
        ),
        (
            "ssh_ssl_user",
            common::parse_ssh_ssl_user,
            MergePolicy::Append,
        ),
        (
            "ssh_deny",
            common::parse_string_list,
            MergePolicy::RejectDuplicate,
        ),
    ] {
        registry.register_directive(context::LOCATION, leaf(name, parser, merge));
    }
    registry.register_directive(
        context::LOCATION,
        raw(
            "types",
            common::parse_types_raw_block,
            MergePolicy::RejectDuplicate,
        ),
    );
}

fn leaf(name: &'static str, parser: DirectiveParserFn, merge: MergePolicy) -> DirectiveSpec {
    DirectiveSpec {
        name,
        allowed_in: vec![context::LOCATION],
        shape: DirectiveShape::Leaf,
        parser,
        merge,
    }
}

fn raw(name: &'static str, parser: DirectiveParserFn, merge: MergePolicy) -> DirectiveSpec {
    DirectiveSpec {
        name,
        allowed_in: vec![context::LOCATION],
        shape: DirectiveShape::RawBlock,
        parser,
        merge,
    }
}

fn finalize_location(
    node: &mut ConfigNode,
    _options: &BuildOptions<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let has_cert = node.get::<PathConfig>("proxy_ssl_certificate")?.is_some();
    let has_key = node
        .get::<PathConfig>("proxy_ssl_certificate_key")?
        .is_some();
    ensure!(
        has_cert == has_key,
        finalize_location_error::ProxyTlsPairSnafu { span: node.span }
    );
    let server_header = HeaderRules(vec![HeaderRule {
        name: HeaderName::from_static("server"),
        value: HeaderValue::from_static("pishoo"),
        always: true,
    }]);
    node.insert_slot("add_header", TypedValue::new(server_header, node.span));
    Ok(())
}

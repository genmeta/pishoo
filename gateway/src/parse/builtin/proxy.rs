use snafu::{Snafu, ensure};

use crate::parse::{
    builtin::common,
    document::ConfigNode,
    registry::{
        BuildOptions, ConfigRegistry, DirectiveParserFn, DirectiveShape, DirectiveSpec,
        MergePolicy, PayloadMode, context,
    },
    source::SourceSpan,
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum FinalizeProxyError {
    #[snafu(display("missing listen directive in proxy context"))]
    MissingListen { span: SourceSpan },
}

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::PROXY,
        finalize: Some(finalize_proxy),
    });
    registry.register_directive(
        context::PISHOO,
        DirectiveSpec {
            name: "proxy",
            allowed_in: vec![context::PISHOO],
            shape: DirectiveShape::ContextBlock {
                child_context: context::PROXY,
                payload: PayloadMode::None,
            },
            parser: common::parse_empty,
            merge: MergePolicy::Append,
        },
    );
    for (name, parser, merge) in [
        (
            "listen",
            common::parse_address as DirectiveParserFn,
            MergePolicy::RejectDuplicate,
        ),
        (
            "client_name",
            common::parse_string,
            MergePolicy::RejectDuplicate,
        ),
        ("dns", common::parse_resolver, MergePolicy::RejectDuplicate),
        (
            "ssl_certificate",
            common::parse_path,
            MergePolicy::RejectDuplicate,
        ),
        (
            "ssl_certificate_key",
            common::parse_path,
            MergePolicy::RejectDuplicate,
        ),
        (
            "allow",
            common::parse_string_list,
            MergePolicy::RejectDuplicate,
        ),
        (
            "deny",
            common::parse_string_list,
            MergePolicy::RejectDuplicate,
        ),
        (
            "default_type",
            common::parse_default_type,
            MergePolicy::RejectDuplicate,
        ),
    ] {
        registry.register_directive(context::PROXY, leaf(name, parser, merge));
    }
    registry.register_directive(
        context::PROXY,
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
        allowed_in: vec![context::PROXY],
        shape: DirectiveShape::Leaf,
        parser,
        merge,
    }
}

fn raw(name: &'static str, parser: DirectiveParserFn, merge: MergePolicy) -> DirectiveSpec {
    DirectiveSpec {
        name,
        allowed_in: vec![context::PROXY],
        shape: DirectiveShape::RawBlock,
        parser,
        merge,
    }
}

fn finalize_proxy(
    node: &mut ConfigNode,
    _options: &BuildOptions<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ensure!(
        !node.get_all_untyped("listen").is_empty(),
        finalize_proxy_error::MissingListenSnafu { span: node.span }
    );
    Ok(())
}

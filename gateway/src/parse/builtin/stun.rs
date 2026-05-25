use crate::parse::{
    builtin::common,
    registry::{
        ConfigRegistry, DirectiveParserFn, DirectiveShape, DirectiveSpec, MergePolicy, PayloadMode,
        context,
    },
};

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::STUN_SERVER,
        finalize: None,
    });
    registry.register_directive(
        context::SERVER,
        DirectiveSpec {
            name: "stun_server",
            allowed_in: vec![context::SERVER],
            shape: DirectiveShape::ContextBlock {
                child_context: context::STUN_SERVER,
                payload: PayloadMode::None,
            },
            parser: common::parse_empty,
            merge: MergePolicy::Append,
        },
    );
    for (name, parser, merge) in [
        (
            "bind",
            common::parse_stun_bind as DirectiveParserFn,
            MergePolicy::Append,
        ),
        (
            "outer_addr",
            common::parse_address,
            MergePolicy::RejectDuplicate,
        ),
        (
            "change_addr",
            common::parse_address,
            MergePolicy::RejectDuplicate,
        ),
        (
            "change_port",
            common::parse_string,
            MergePolicy::RejectDuplicate,
        ),
    ] {
        registry.register_directive(context::STUN_SERVER, leaf(name, parser, merge));
    }
}

fn leaf(name: &'static str, parser: DirectiveParserFn, merge: MergePolicy) -> DirectiveSpec {
    DirectiveSpec {
        name,
        allowed_in: vec![context::STUN_SERVER],
        shape: DirectiveShape::Leaf,
        parser,
        merge,
    }
}

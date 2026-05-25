use crate::parse::{
    builtin::common,
    registry::{
        ConfigRegistry, DirectiveParserFn, DirectiveShape, DirectiveSpec, MergePolicy, PayloadMode,
        context,
    },
};

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::PISHOO,
        finalize: None,
    });
    registry.register_directive(
        context::ROOT,
        DirectiveSpec {
            name: "pishoo",
            allowed_in: vec![context::ROOT],
            shape: DirectiveShape::ContextBlock {
                child_context: context::PISHOO,
                payload: PayloadMode::None,
            },
            parser: common::parse_empty,
            merge: MergePolicy::Append,
        },
    );
    for (name, parser) in [
        ("pid", common::parse_string as DirectiveParserFn),
        ("workers", common::parse_string_list),
        ("groups", common::parse_string_list),
        ("access_rules", common::parse_string),
        ("gzip", common::parse_boolean),
        ("gzip_vary", common::parse_boolean),
        ("gzip_min_length", common::parse_string),
        ("gzip_comp_level", common::parse_string),
        ("gzip_types", common::parse_string_list),
        ("default_type", common::parse_default_type),
    ] {
        registry.register_directive(
            context::PISHOO,
            leaf(name, parser, MergePolicy::RejectDuplicate),
        );
    }
    registry.register_directive(
        context::PISHOO,
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
        allowed_in: vec![context::PISHOO],
        shape: DirectiveShape::Leaf,
        parser,
        merge,
    }
}

fn raw(name: &'static str, parser: DirectiveParserFn, merge: MergePolicy) -> DirectiveSpec {
    DirectiveSpec {
        name,
        allowed_in: vec![context::PISHOO],
        shape: DirectiveShape::RawBlock,
        parser,
        merge,
    }
}

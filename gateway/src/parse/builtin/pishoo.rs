use crate::parse::{
    registry::{ConfigRegistry, DirectiveSpec, MergePolicy, context},
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, MimeTypes,
        PathConfig, StringList,
    },
};

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::PISHOO,
        finalize: None,
    });
    registry.register_directive(
        context::ROOT,
        DirectiveSpec::context_empty(
            "pishoo",
            vec![context::ROOT],
            context::PISHOO,
            MergePolicy::Append,
        ),
    );
    register_leaf::<PathConfig>(registry, "pid");
    register_leaf::<StringList>(registry, "workers");
    register_leaf::<StringList>(registry, "groups");
    register_leaf::<AccessRulesUri>(registry, "access_rules");
    register_leaf::<BoolConfig>(registry, "gzip");
    register_leaf::<BoolConfig>(registry, "gzip_vary");
    register_leaf::<GzipMinLength>(registry, "gzip_min_length");
    register_leaf::<GzipCompLevel>(registry, "gzip_comp_level");
    register_leaf::<StringList>(registry, "gzip_types");
    register_leaf::<DefaultType>(registry, "default_type");
    registry.register_directive(
        context::PISHOO,
        DirectiveSpec::raw_value::<MimeTypes>(
            "types",
            vec![context::PISHOO],
            MergePolicy::RejectDuplicate,
        ),
    );
}

fn register_leaf<T>(registry: &mut ConfigRegistry, name: &'static str)
where
    T: crate::parse::registry::DirectiveValue,
    for<'input, 'directive> T: TryFrom<
            &'input crate::parse::registry::DirectiveInput<'directive>,
            Error = <T as crate::parse::registry::DirectiveValue>::Error,
        >,
{
    registry.register_directive(
        context::PISHOO,
        DirectiveSpec::leaf_value::<T>(name, vec![context::PISHOO], MergePolicy::RejectDuplicate),
    );
}

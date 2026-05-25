use snafu::{Snafu, ensure};

use crate::parse::{
    document::ConfigNode,
    registry::{BuildOptions, ConfigRegistry, DirectiveSpec, MergePolicy, context},
    source::SourceSpan,
    types::{
        ClientNameConfig, DefaultType, MimeTypes, PathConfig, ResolverConfig, SocketAddrs,
        StringList,
    },
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
        DirectiveSpec::context_empty(
            "proxy",
            vec![context::PISHOO],
            context::PROXY,
            MergePolicy::Append,
        ),
    );
    register_leaf::<SocketAddrs>(registry, "listen");
    register_leaf::<ClientNameConfig>(registry, "client_name");
    register_leaf::<ResolverConfig>(registry, "dns");
    register_leaf::<PathConfig>(registry, "ssl_certificate");
    register_leaf::<PathConfig>(registry, "ssl_certificate_key");
    register_leaf::<StringList>(registry, "allow");
    register_leaf::<StringList>(registry, "deny");
    register_leaf::<DefaultType>(registry, "default_type");
    registry.register_directive(
        context::PROXY,
        DirectiveSpec::raw_value::<MimeTypes>(
            "types",
            vec![context::PROXY],
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
        context::PROXY,
        DirectiveSpec::leaf_value::<T>(name, vec![context::PROXY], MergePolicy::RejectDuplicate),
    );
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

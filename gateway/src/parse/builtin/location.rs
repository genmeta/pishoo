use http::{HeaderName, HeaderValue};
use snafu::{Snafu, ensure};

use crate::parse::{
    document::ConfigNode,
    pattern::{ParsePatternError, Pattern},
    registry::{
        BuildOptions, ConfigRegistry, DirectiveInput, DirectiveSpec, DirectiveValue, MergePolicy,
        context,
    },
    source::SourceSpan,
    types::{
        BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, HeaderRule, HeaderRules, MimeTypes,
        PathConfig, ProxyPass, SshLoginMethods, SshSslUsers, StringList,
    },
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

impl DirectiveValue for Pattern {
    type Error = ParsePatternError;
}

impl<'input, 'directive> TryFrom<&'input DirectiveInput<'directive>> for Pattern {
    type Error = ParsePatternError;

    fn try_from(input: &'input DirectiveInput<'directive>) -> Result<Self, Self::Error> {
        crate::parse::pattern::parse_spanned_pattern(&input.directive.args)
    }
}

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::LOCATION,
        finalize: Some(finalize_location),
    });
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::context_payload::<Pattern>(
            "location",
            vec![context::SERVER],
            context::LOCATION,
            MergePolicy::Append,
        ),
    );
    register_leaf::<PathConfig>(registry, "root", MergePolicy::RejectDuplicate);
    register_leaf::<PathConfig>(registry, "alias", MergePolicy::RejectDuplicate);
    register_leaf::<BoolConfig>(registry, "gzip", MergePolicy::RejectDuplicate);
    register_leaf::<BoolConfig>(registry, "gzip_vary", MergePolicy::RejectDuplicate);
    register_leaf::<GzipMinLength>(registry, "gzip_min_length", MergePolicy::RejectDuplicate);
    register_leaf::<GzipCompLevel>(registry, "gzip_comp_level", MergePolicy::RejectDuplicate);
    register_leaf::<StringList>(registry, "gzip_types", MergePolicy::RejectDuplicate);
    register_leaf::<StringList>(registry, "index", MergePolicy::RejectDuplicate);
    register_leaf::<HeaderRules>(registry, "add_header", MergePolicy::Append);
    register_leaf::<HeaderRules>(registry, "proxy_set_header", MergePolicy::Append);
    register_leaf::<ProxyPass>(registry, "proxy_pass", MergePolicy::RejectDuplicate);
    register_leaf::<PathConfig>(
        registry,
        "proxy_ssl_certificate",
        MergePolicy::RejectDuplicate,
    );
    register_leaf::<PathConfig>(
        registry,
        "proxy_ssl_certificate_key",
        MergePolicy::RejectDuplicate,
    );
    register_leaf::<PathConfig>(
        registry,
        "proxy_ssl_trusted_certificate",
        MergePolicy::RejectDuplicate,
    );
    register_leaf::<SshLoginMethods>(registry, "ssh_login", MergePolicy::RejectDuplicate);
    register_leaf::<SshSslUsers>(registry, "ssh_ssl_user", MergePolicy::Append);
    register_leaf::<StringList>(registry, "ssh_deny", MergePolicy::RejectDuplicate);
    register_leaf::<DefaultType>(registry, "default_type", MergePolicy::RejectDuplicate);
    registry.register_directive(
        context::LOCATION,
        DirectiveSpec::raw_value::<MimeTypes>(
            "types",
            vec![context::LOCATION],
            MergePolicy::RejectDuplicate,
        ),
    );
}

fn register_leaf<T>(registry: &mut ConfigRegistry, name: &'static str, merge: MergePolicy)
where
    T: crate::parse::registry::DirectiveValue,
    for<'input, 'directive> T: TryFrom<
            &'input crate::parse::registry::DirectiveInput<'directive>,
            Error = <T as crate::parse::registry::DirectiveValue>::Error,
        >,
{
    registry.register_directive(
        context::LOCATION,
        DirectiveSpec::leaf_value::<T>(name, vec![context::LOCATION], merge),
    );
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

use std::path::PathBuf;

use dhttp_config::identity::ssl::{CERT_FILE_NAME, KEY_FILE_NAME};
use snafu::{OptionExt, Snafu, ensure};

use crate::parse::{
    document::ConfigNode,
    registry::{BuildOptions, ConfigRegistry, ContextKey, DirectiveSpec, MergePolicy, context},
    source::SourceSpan,
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, ListenConfig,
        MimeTypes, PathConfig, ResolverConfig, ServerIdConfig, ServerName, ServerNames, StringList,
    },
    value::TypedValue,
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum FinalizeServerError {
    #[snafu(display("missing listen directive in server context"))]
    MissingListen { span: SourceSpan },
    #[snafu(display("missing ssl_certificate directive in server context"))]
    MissingCertificate { span: SourceSpan },
    #[snafu(display("missing ssl_certificate_key directive in server context"))]
    MissingCertificateKey { span: SourceSpan },
    #[snafu(display("default ssl_certificate path does not exist"))]
    MissingDefaultCertificate { span: SourceSpan, path: PathBuf },
    #[snafu(display("default ssl_certificate_key path does not exist"))]
    MissingDefaultCertificateKey { span: SourceSpan, path: PathBuf },
}

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::SERVER,
        finalize: Some(finalize_server),
    });
    registry.register_directive(context::ROOT, server_block(context::ROOT));
    registry.register_directive(context::PISHOO, server_block(context::PISHOO));
    register_server_leaf::<ListenConfig>(registry, "listen", MergePolicy::Append);
    register_server_leaf::<ServerNames>(registry, "server_name", MergePolicy::RejectDuplicate);
    register_server_leaf::<ServerIdConfig>(registry, "server_id", MergePolicy::RejectDuplicate);
    register_server_leaf::<ResolverConfig>(registry, "dns", MergePolicy::RejectDuplicate);
    register_server_leaf::<BoolConfig>(registry, "gzip", MergePolicy::RejectDuplicate);
    register_server_leaf::<BoolConfig>(registry, "gzip_vary", MergePolicy::RejectDuplicate);
    register_server_leaf::<GzipMinLength>(
        registry,
        "gzip_min_length",
        MergePolicy::RejectDuplicate,
    );
    register_server_leaf::<GzipCompLevel>(
        registry,
        "gzip_comp_level",
        MergePolicy::RejectDuplicate,
    );
    register_server_leaf::<StringList>(registry, "gzip_types", MergePolicy::RejectDuplicate);
    register_server_leaf::<PathConfig>(registry, "ssl_certificate", MergePolicy::RejectDuplicate);
    register_server_leaf::<PathConfig>(
        registry,
        "ssl_certificate_key",
        MergePolicy::RejectDuplicate,
    );
    register_server_leaf::<DefaultType>(registry, "default_type", MergePolicy::RejectDuplicate);
    register_server_leaf::<AccessRulesUri>(registry, "access_rules", MergePolicy::RejectDuplicate);
    register_server_leaf::<BoolConfig>(registry, "relay", MergePolicy::RejectDuplicate);
    register_server_leaf::<BoolConfig>(registry, "stun", MergePolicy::RejectDuplicate);
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::raw_value::<MimeTypes>(
            "types",
            vec![context::SERVER],
            MergePolicy::RejectDuplicate,
        ),
    );
}

fn server_block(parent: ContextKey) -> DirectiveSpec {
    DirectiveSpec::context_empty("server", vec![parent], context::SERVER, MergePolicy::Append)
}

fn register_server_leaf<T>(registry: &mut ConfigRegistry, name: &'static str, merge: MergePolicy)
where
    T: crate::parse::registry::DirectiveValue,
    for<'input, 'directive> T: TryFrom<
            &'input crate::parse::registry::DirectiveInput<'directive>,
            Error = <T as crate::parse::registry::DirectiveValue>::Error,
        >,
{
    registry.register_directive(
        context::SERVER,
        DirectiveSpec::leaf_value::<T>(name, vec![context::SERVER], merge),
    );
}

fn finalize_server(
    node: &mut ConfigNode,
    options: &BuildOptions<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ensure!(
        !node.get_all::<ListenConfig>("listen")?.is_empty(),
        finalize_server_error::MissingListenSnafu { span: node.span }
    );
    if let Some(identity_home) = options.identity_home {
        if node.get::<ServerNames>("server_name")?.is_none() {
            node.insert_slot(
                "server_name",
                TypedValue::new(
                    ServerNames(vec![ServerName {
                        name: identity_home.name().to_owned(),
                    }]),
                    node.span,
                ),
            );
        }
        if node.get::<PathConfig>("ssl_certificate")?.is_none() {
            let path = identity_home.ssl_dir().join(CERT_FILE_NAME);
            ensure!(
                path.exists(),
                finalize_server_error::MissingDefaultCertificateSnafu {
                    span: node.span,
                    path
                }
            );
            node.insert_slot(
                "ssl_certificate",
                TypedValue::new(PathConfig(path), node.span),
            );
        }
        if node.get::<PathConfig>("ssl_certificate_key")?.is_none() {
            let path = identity_home.ssl_dir().join(KEY_FILE_NAME);
            ensure!(
                path.exists(),
                finalize_server_error::MissingDefaultCertificateKeySnafu {
                    span: node.span,
                    path
                }
            );
            node.insert_slot(
                "ssl_certificate_key",
                TypedValue::new(PathConfig(path), node.span),
            );
        }
    }
    node.get::<PathConfig>("ssl_certificate")?
        .context(finalize_server_error::MissingCertificateSnafu { span: node.span })?;
    node.get::<PathConfig>("ssl_certificate_key")?
        .context(finalize_server_error::MissingCertificateKeySnafu { span: node.span })?;
    Ok(())
}

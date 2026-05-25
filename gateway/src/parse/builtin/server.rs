use std::path::PathBuf;

use dhttp_config::identity::ssl::{CERT_FILE_NAME, KEY_FILE_NAME};
use snafu::{OptionExt, Snafu, ensure};

use crate::parse::{
    builtin::common,
    document::ConfigNode,
    registry::{
        BuildOptions, ConfigRegistry, DirectiveParserFn, DirectiveShape, DirectiveSpec,
        MergePolicy, PayloadMode, context,
    },
    source::SourceSpan,
    types::{ListenConfig, PathConfig, ServerName, ServerNames},
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
    for context in [context::SERVER] {
        for (name, parser, merge) in [
            (
                "listen",
                common::parse_listen as DirectiveParserFn,
                MergePolicy::Append,
            ),
            (
                "server_name",
                common::parse_server_name,
                MergePolicy::RejectDuplicate,
            ),
            (
                "server_id",
                common::parse_server_id,
                MergePolicy::RejectDuplicate,
            ),
            ("dns", common::parse_resolver, MergePolicy::RejectDuplicate),
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
                "default_type",
                common::parse_default_type,
                MergePolicy::RejectDuplicate,
            ),
            (
                "access_rules",
                common::parse_string,
                MergePolicy::RejectDuplicate,
            ),
            ("relay", common::parse_boolean, MergePolicy::RejectDuplicate),
            ("stun", common::parse_boolean, MergePolicy::RejectDuplicate),
        ] {
            registry.register_directive(context, leaf(context, name, parser, merge));
        }
        registry.register_directive(
            context,
            raw(
                context,
                "types",
                common::parse_types_raw_block,
                MergePolicy::RejectDuplicate,
            ),
        );
    }
}

fn server_block(parent: crate::parse::registry::ContextKey) -> DirectiveSpec {
    DirectiveSpec {
        name: "server",
        allowed_in: vec![parent],
        shape: DirectiveShape::ContextBlock {
            child_context: context::SERVER,
            payload: PayloadMode::None,
        },
        parser: common::parse_empty,
        merge: MergePolicy::Append,
    }
}

fn leaf(
    context: crate::parse::registry::ContextKey,
    name: &'static str,
    parser: DirectiveParserFn,
    merge: MergePolicy,
) -> DirectiveSpec {
    DirectiveSpec {
        name,
        allowed_in: vec![context],
        shape: DirectiveShape::Leaf,
        parser,
        merge,
    }
}

fn raw(
    context: crate::parse::registry::ContextKey,
    name: &'static str,
    parser: DirectiveParserFn,
    merge: MergePolicy,
) -> DirectiveSpec {
    DirectiveSpec {
        name,
        allowed_in: vec![context],
        shape: DirectiveShape::RawBlock,
        parser,
        merge,
    }
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

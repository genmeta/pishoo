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

#[cfg(test)]
mod tests {
    use crate::parse::{
        tests::{cleanup_temp_files, create_temp_file, first_pishoo, parse_doc},
        types::{ClientNameConfig, DefaultType, MimeTypes, ResolverConfig, StringList},
    };

    #[test]
    fn parse_client_name_directive_and_rejects_invalid_name() {
        let cert = create_temp_file("client_name_cert");
        let key = create_temp_file("client_name_key");
        let conf = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; }} proxy {{ listen 127.0.0.1:8080; client_name good.example.com; }} }}",
            cert.display(),
            key.display()
        );

        let proxy = first_pishoo(&parse_doc(&conf))
            .children("proxy")
            .expect("proxy should exist")[0]
            .clone();

        assert_eq!(
            proxy
                .require::<ClientNameConfig>("client_name")
                .expect("client_name should be typed")
                .0
                .as_partial(),
            "good.example.com"
        );

        cleanup_temp_files(&[&cert, &key]);
    }

    #[test]
    fn parse_client_name_rejects_invalid_value() {
        let conf = "pishoo { proxy { listen 127.0.0.1:8080; client_name not host@@; } }";

        let failure = crate::parse::parse_config_str_for_test(conf)
            .expect_err("invalid client_name should fail");

        let report = snafu::Report::from_error(&failure.error).to_string();
        assert!(
            report.contains("invalid client_name directive value")
                || report.contains("invalid dhttp name")
                || report.contains("failed to parse directive `client_name`")
        );
    }

    #[test]
    fn parse_default_type_in_proxy_context() {
        let conf = "pishoo { proxy { listen 127.0.0.1:8080; default_type text/plain; } }";

        let proxy = first_pishoo(&parse_doc(conf))
            .children("proxy")
            .expect("proxy exists")[0]
            .clone();

        assert_eq!(
            proxy
                .require::<DefaultType>("default_type")
                .expect("default_type should be typed")
                .0
                .to_str()
                .unwrap(),
            "text/plain"
        );
    }

    #[test]
    fn parse_mime_types_in_proxy_context() {
        let conf =
            "pishoo { proxy { listen 127.0.0.1:8080; types { text/plain txt; text/css css; } } }";

        let proxy = first_pishoo(&parse_doc(conf))
            .children("proxy")
            .expect("proxy exists")[0]
            .clone();

        let types = proxy
            .require::<MimeTypes>("types")
            .expect("types should be typed")
            .0
            .clone();

        assert_eq!(types.get("txt").unwrap().to_str().unwrap(), "text/plain");
        assert_eq!(types.get("css").unwrap().to_str().unwrap(), "text/css");
    }

    #[test]
    fn parse_proxy_allow_and_deny_collects_values() {
        let conf = "pishoo { proxy { listen 127.0.0.1:8080; allow admin viewer; deny none; } }";

        let proxy = first_pishoo(&parse_doc(conf))
            .children("proxy")
            .expect("proxy exists")[0]
            .clone();

        assert_eq!(
            proxy
                .require::<StringList>("allow")
                .expect("allow should be typed")
                .0,
            vec!["admin", "viewer"]
        );
        assert_eq!(
            proxy
                .require::<StringList>("deny")
                .expect("deny should be typed")
                .0,
            vec!["none"]
        );
    }

    #[test]
    fn parse_proxy_dns_directive_accepts_h3_uri() {
        let conf =
            "pishoo { proxy { listen 127.0.0.1:8080; dns h3 https://dns.genmeta.net/dns-query; } }";

        let proxy = first_pishoo(&parse_doc(conf))
            .children("proxy")
            .expect("proxy exists")[0]
            .clone();

        assert_eq!(
            proxy
                .require::<ResolverConfig>("dns")
                .expect("dns should be typed")
                .0
                .to_string(),
            "https://dns.genmeta.net/dns-query"
        );
    }

    #[test]
    fn parse_proxy_ssl_certificate_and_key_path_types() {
        let client_cert = create_temp_file("proxy_client_ssl_cert");
        let client_key = create_temp_file("proxy_client_ssl_key");

        let conf = format!(
            "pishoo {{ proxy {{ listen 127.0.0.1:8080; ssl_certificate {}; ssl_certificate_key {}; }} }}",
            client_cert.display(),
            client_key.display()
        );

        let proxy = first_pishoo(&parse_doc(&conf))
            .children("proxy")
            .expect("proxy exists")[0]
            .clone();

        assert_eq!(
            proxy
                .require::<crate::parse::types::PathConfig>("ssl_certificate")
                .expect("proxy ssl certificate should be typed")
                .0,
            client_cert.clone()
        );
        assert_eq!(
            proxy
                .require::<crate::parse::types::PathConfig>("ssl_certificate_key")
                .expect("proxy ssl certificate key should be typed")
                .0,
            client_key.clone()
        );

        cleanup_temp_files(&[&client_cert, &client_key]);
    }
}

use std::{collections::HashMap, path::Path, sync::Arc};

use http::{HeaderValue, Request, header, response::Parts};

use super::variables;
use crate::parse::{
    document::ConfigNode,
    types::{DefaultType, HeaderRule, HeaderRules, MimeTypes, ProxyPass},
};

pub(crate) fn proxy_set_header<T>(node: &Arc<ConfigNode>, req: Request<T>) -> Request<T> {
    let (mut parts, body) = req.into_parts();

    // 默认将 Host 变更为 proxy_pass target
    let proxy_host = node
        .get::<ProxyPass>("proxy_pass")
        .ok()
        .flatten()
        .map(|proxy_pass| proxy_pass.proxy_host.clone());
    if let Some(host) = proxy_host {
        parts.headers.insert(
            header::HOST,
            host.parse()
                .unwrap_or_else(|_| HeaderValue::from_static("localhost")),
        );
    };

    // 默认将 Connection 变更为 close
    parts
        .headers
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    // 遍历 proxy_set_header 中的记录, 匹配 Header, 设置支持的字段
    let proxy_set_header = header_rules(node, "proxy_set_header");

    for HeaderRule {
        name,
        value,
        always: _,
    } in proxy_set_header
    {
        // 匹配变量进行转换
        // TODO 变量拼接
        parts.headers.insert(name, variables::search(&parts, value));
    }

    Request::from_parts(parts, body)
}

/// Adds headers to the HTTP response parts based on configuration in the node.
///
/// Reads header directives from the `add_header` field within the `node`.
/// Headers are added to `parts.headers` if the response status in `parts.status`
/// is success (2xx) or redirection (3xx), or if the specific header directive
/// is marked with an 'always' flag.
///
/// # Arguments
///
/// * `node` - A config node potentially containing header configurations under the key "add_header".
/// * `parts` - A mutable reference to `http::response::Parts` where headers will be added.
pub(crate) fn add_header(node: &Arc<ConfigNode>, parts: &mut Parts) {
    let add_header = header_rules(node, "add_header");

    for HeaderRule {
        name,
        value,
        always,
    } in add_header
    {
        if parts.status.is_success() || parts.status.is_redirection() || always {
            parts.headers.insert(name, value);
        }
    }
}

/// Determines and sets the "Content-Type" header for a given file path based on configuration.
pub(crate) fn content_type(node: &Arc<ConfigNode>, parts: &mut Parts, file_path: &Path) {
    let mime_types = node.inherited::<MimeTypes>("types").ok().flatten();
    let default_type = node.inherited::<DefaultType>("default_type").ok().flatten();

    if let Some(mime_types) = mime_types
        && let Some(content_type) = infer_content_type(
            file_path,
            &mime_types.0,
            default_type.as_ref().map(|v| &v.0),
        )
    {
        parts.headers.insert("Content-Type", content_type.clone());
    }
}

fn header_rules(node: &ConfigNode, name: &str) -> Vec<HeaderRule> {
    node.get_all::<HeaderRules>(name)
        .ok()
        .into_iter()
        .flatten()
        .flat_map(|headers| headers.0.clone())
        .collect()
}

/// Infers the `Content-Type` `HeaderValue` for a given file path based on its extension.
fn infer_content_type<'a>(
    file_path: &Path,
    mime_types: &'a HashMap<String, HeaderValue>,
    default_type: Option<&'a HeaderValue>,
) -> Option<&'a HeaderValue> {
    let Some(ext) = file_path.extension().and_then(|ext| ext.to_str()) else {
        return default_type;
    };
    let ext = ext.to_lowercase();
    match mime_types.get(&ext) {
        Some(content_type) => Some(content_type),
        None => default_type,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{
        document::ConfigNode,
        registry::context,
        source::{SourceId, SourceSpan},
        value::TypedValue,
    };

    #[test]
    fn proxy_set_header_defaults_host_to_proxy_host_with_port() {
        let span = SourceSpan::new(SourceId(0), 0, 0);
        let mut node = ConfigNode::new(context::LOCATION, None, span);
        node.insert_slot(
            "proxy_pass",
            TypedValue::new(
                ProxyPass {
                    raw: "http://backend.example.com:8080/base/".to_string(),
                    uri: "http://backend.example.com:8080/base/".parse().unwrap(),
                    proxy_host: "backend.example.com:8080".to_string(),
                    explicit_path_and_query: Some("/base/".to_string()),
                },
                span,
            ),
        );

        let req = http::Request::builder().uri("/echo").body(()).unwrap();
        let req = proxy_set_header(&Arc::new(node), req);
        assert_eq!(
            req.headers()[http::header::HOST],
            "backend.example.com:8080"
        );
    }
}

use std::{collections::HashMap, sync::Arc};

use http::{HeaderValue, Request, Uri, header, response::Parts};

use super::variables;
use crate::parse::{Node, Value};

pub(crate) fn proxy_set_header<T>(node: &Arc<Node>, req: Request<T>) -> Request<T> {
    let (mut parts, body) = req.into_parts();

    // 默认将 Host 变更为 proxy_pass target
    let proxy_host = match node.get("proxy_pass") {
        Some(Value::Uri(uri)) => uri.host().map(|h| h.to_string()),
        Some(Value::String(s)) => s
            .parse::<Uri>()
            .ok()
            .and_then(|u| u.host().map(|h| h.to_string())),
        _ => None,
    };
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
    let proxy_set_header = if let Some(Value::Header(header)) = node.get("proxy_set_header") {
        header.clone()
    } else {
        Vec::new()
    };

    for (header, value, _) in proxy_set_header {
        // 匹配变量进行转换
        // TODO 变量拼接
        parts
            .headers
            .insert(header, variables::search(&parts, value));
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
/// * `node` - An `Arc<Node>` potentially containing header configurations under the key "add_header".
/// * `parts` - A mutable reference to `http::response::Parts` where headers will be added.
pub(crate) fn add_header(node: &Arc<Node>, parts: &mut Parts) {
    let add_header = if let Some(Value::Header(header)) = node.get("add_header") {
        header
    } else {
        &Vec::new()
    };

    for (header, value, always) in add_header {
        if parts.status.is_success() || parts.status.is_redirection() || *always {
            parts.headers.insert(header, value.clone());
        }
    }
}

/// Determines and sets the "Content-Type" header for a given file path based on configuration.
pub(crate) fn content_type(node: &Arc<Node>, parts: &mut Parts, file_path: &str) {
    let mime_types = node.get_types("types");
    let default_type = node.get_header_value("default_type");

    if let Some(mime_types) = mime_types
        && let Some(content_type) =
            infer_content_type(file_path, &mime_types, default_type.as_ref())
    {
        parts.headers.insert("Content-Type", content_type.clone());
    }
}

/// Infers the `Content-Type` `HeaderValue` for a given file path based on its extension.
fn infer_content_type<'a>(
    file_path: &str,
    mime_types: &'a HashMap<String, HeaderValue>,
    default_type: Option<&'a HeaderValue>,
) -> Option<&'a HeaderValue> {
    let ext = match file_path.rsplit(".").next() {
        Some(ext) => ext.to_lowercase(),
        None => return default_type,
    };
    match mime_types.get(&ext) {
        Some(content_type) => Some(content_type),
        None => default_type,
    }
}

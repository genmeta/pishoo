use std::{collections::HashMap, io, sync::Arc};

use acl::Acl;
use http::{HeaderMap, HeaderValue, Request, Uri, header, response::Parts};
use tokio::fs::File;

use crate::parse::{Node, Value};

pub(crate) mod acl;
pub(crate) mod variables;

/// Attempts to open a file directly or serve an index file if the path points to a directory.
///
/// If `file_path` refers to a regular file, it opens that file.
/// If `file_path` refers to a directory, it searches for index files within that directory
/// based on the configuration found in the `node` (specifically looking for a "index" key
/// under an "index" sub-node). It attempts to open the first valid index file found.
///
/// # Arguments
///
/// * `node` - An `Arc<Node>` used to retrieve the list of potential index filenames.
/// * `file_path` - The path to the file or directory to serve.
///
/// # Returns
///
/// Returns a `Result` containing a tuple `(File, u64)` on success, where `File` is the
/// opened file handle and `u64` is the file size.
/// Returns an `io::Error` if the path doesn't exist, if it's a directory without a
/// suitable index file, or if file/metadata operations fail.
pub(crate) async fn index(
    node: &Arc<Node>,
    file_path: impl Into<String>,
) -> io::Result<(File, u64, String)> {
    let mut file_path = file_path.into();
    let metadata = tokio::fs::metadata(&file_path).await?;

    if metadata.is_file() {
        return File::open(&file_path)
            .await
            .map(|file| (file, metadata.len(), file_path));
    }

    // 2. 检查是否是目录
    if metadata.is_dir() {
        if !file_path.ends_with('/') {
            file_path.push('/');
        }
        let base_dir_path = file_path.clone();

        let node_found = node.backtrack_node("index");
        let index_files = node_found.as_ref().map(|node| {
            if let Some(Value::StringVec(index_files)) = node.get("index") {
                index_files
            } else {
                unreachable!("Invalid index value")
            }
        });

        let index_files = if let Some(index_files) = index_files {
            index_files
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No index files found while trying to serve directory",
            ));
        };

        for index_filename in index_files {
            let mut potential_path = base_dir_path.clone();
            potential_path.push_str(index_filename);

            if let Ok(metadata) = tokio::fs::metadata(&*potential_path).await
                && metadata.is_file()
            {
                return File::open(&*potential_path)
                    .await
                    .map(|file| (file, metadata.len(), potential_path));
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("File not found: {file_path}"),
    ))
}

pub(crate) fn proxy_set_header<T>(node: &Arc<Node>, req: Request<T>) -> Request<T> {
    let (mut parts, body) = req.into_parts();
    let mut new_headers = HeaderMap::new();

    // 默认将 Host 变更为 proxy_pass target
    if let Some(Value::String(uri)) = node.get("proxy_pass") {
        new_headers.insert(
            header::HOST,
            uri.parse::<Uri>()
                .unwrap()
                .host()
                .unwrap_or_default()
                .to_string()
                .parse()
                .unwrap_or_else(|_| HeaderValue::from_static("localhost")),
        );
    };

    // 默认将 Connection 变更为 close
    new_headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    // 遍历 proxy_set_header 中的记录, 匹配 Header, 设置支持的字段
    let proxy_set_header = if let Some(Value::Header(header)) = node.get("proxy_set_header") {
        header.clone()
    } else {
        Vec::new()
    };

    for (header, value, _) in proxy_set_header {
        // 匹配变量进行转换
        // TODO 变量拼接
        new_headers.insert(header, variables::search(&parts, value));
    }

    parts.headers = new_headers;
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
///
/// This function retrieves MIME type mapping and a default type from configuration nodes
/// found by searching upwards from the provided `node` using `backtrack_node`.
/// It uses the `infer_content_type` function to determine the appropriate `Content-Type`
/// based on the file extension of `file_path`. If a type is inferred, it's added
/// to the `parts.headers`.
///
/// # Arguments
///
/// * `node` - The context `Node` potentially containing or inheriting `types` and `default_type` configurations.
/// * `parts` - A mutable representation of response parts (e.g., headers) where the
///   `Content-Type` header will be set.
/// * `file_path` - The path of the file whose content type needs to be determined.
///
/// # Panics
///
/// Panics with `unreachable!` if a configuration node for `types` or `default_type` is found
/// but contains a value of an unexpected type, indicating invalid configuration data.
pub(crate) fn content_type(node: &Arc<Node>, parts: &mut Parts, file_path: &str) {
    let node_found = node.backtrack_node("types");
    let mime_types = node_found.as_ref().map(|node| {
        if let Some(Value::Types(mime_types)) = node.get("types") {
            mime_types
        } else {
            unreachable!("Invalid mime_types value")
        }
    });

    let node_found = node.backtrack_node("default_type");
    let default_type = node_found.as_ref().map(|node| {
        if let Some(Value::HeaderValue(default_type)) = node.get("default_type") {
            default_type
        } else {
            unreachable!("Invalid default_type value")
        }
    });

    if let Some(mime_types) = mime_types
        && let Some(content_type) = infer_content_type(file_path, mime_types, default_type)
    {
        parts.headers.insert("Content-Type", content_type.clone());
    }
}

/// Infers the `Content-Type` `HeaderValue` for a given file path based on its extension.
///
/// # Arguments
///
/// * `file_path` - The path to the file. The extension is extracted from this path.
/// * `mime_types` - A map where keys are lowercase file extensions (e.g., "txt", "html")
///   and values are the corresponding `HeaderValue` MIME types.
/// * `default_type` - An optional default `HeaderValue` to return if the file has no
///   extension or if the extension is not found in the `mime_types` map.
///
/// # Returns
///
/// Returns `Some` containing a reference to the corresponding `HeaderValue` from `mime_types`
/// if the extension is found. Otherwise, returns the `default_type` (which could be `None`).
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

pub(crate) fn acl(node: &Arc<Node>) -> Acl {
    let node_found = node.backtrack_node("allow");
    let allow = node_found.as_ref().map(|node| {
        if let Some(Value::StringVec(allow)) = node.get("allow") {
            allow
        } else {
            unreachable!("Invalid allow value")
        }
    });
    let allow = if let Some(allow) = allow {
        allow
    } else {
        &Vec::new()
    };

    let node_found = node.backtrack_node("deny");
    let deny = node_found.as_ref().map(|node| {
        if let Some(Value::StringVec(deny)) = node.get("deny") {
            deny
        } else {
            unreachable!("Invalid deny value")
        }
    });
    let deny = if let Some(deny) = deny {
        deny
    } else {
        &Vec::new()
    };

    let allow = acl::parse_host_matches(allow);
    let deny = acl::parse_host_matches(deny);

    Acl::new(allow, deny)
}

use std::collections::HashMap;

use bytes::Bytes;
use h3::server::RequestStream;
use h3_shim::SendStream;
use http::{Request, Response, Uri};
use tokio::io::{AsyncReadExt, BufReader};
use tracing::{debug, error, info};

use crate::{error::Result, parse::location::FileLocation};

/// 处理 ROOT 静态文件请求
pub async fn root(
    location: &FileLocation,
    req: Request<()>,
    sender: &mut RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    // 对于 Root 类型，把请求路径直接拼接到根目录上
    let mut file_path = format!("{}{}", location.replace, req.uri().path());
    // 处理 index 文件
    index(&mut file_path, &location.index).await?;

    serve_static_file(
        &file_path,
        req.uri(),
        &location.mime_types,
        &location.default_type,
        sender,
    )
    .await?;
    Ok(())
}

/// 处理 ALIAS 静态文件请求
pub async fn alias(
    location: &FileLocation,
    pattern: String,
    req: Request<()>,
    sender: &mut RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    // 在 alias 情况下，需要去除 URL 中匹配到的前缀
    let relative_path = req.uri().path().trim_start_matches(&pattern);
    let mut file_path = format!("{}{}", location.replace, relative_path);

    index(&mut file_path, &location.index).await?;

    serve_static_file(
        &file_path,
        req.uri(),
        &location.mime_types,
        &location.default_type,
        sender,
    )
    .await?;
    Ok(())
}

/// 异步加载文件，并以流式方式通过 sender 发送响应体。
async fn serve_static_file(
    file_path: &str,
    uri: &Uri,
    mime_types: &HashMap<String, String>,
    default_type: &Option<String>,
    sender: &mut RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    match tokio::fs::File::open(file_path).await {
        Ok(file) => {
            // 获取文件元数据，用于确定文件大小
            let metadata = match tokio::fs::metadata(file_path).await {
                Ok(metadata) => metadata,
                Err(e) => {
                    error!(
                        "[Static file serving][{}] Failed to get metadata: {:?}",
                        uri, e
                    );
                    sender.send_response(build_unfound_response()?).await?;
                    return Ok(());
                }
            };

            let content_length = metadata.len();
            let content_type = infer_content_type(file_path, mime_types, default_type.as_ref());

            let response = Response::builder()
                .status(200)
                .header("Content-Length", content_length);

            let response = match content_type {
                Some(content_type) => response.header("Content-Type", content_type),
                None => {
                    error!(
                        "[Static file serving][{}] Failed to infer content type for {}",
                        uri, file_path
                    );
                    response
                }
            };

            let response = response.body(()).inspect_err(|e| {
                error!(
                    "[Static file serving][{}] Failed to build response: {}",
                    uri, e
                )
            })?;
            sender.send_response(response).await?;

            debug!(
                "[Static file serving][{}] Serving file {} ({} bytes)",
                uri, file_path, content_length
            );

            let mut reader = BufReader::new(file);
            let mut buffer = [0u8; 8192];
            while reader.read(&mut buffer).await? > 0 {
                sender.send_data(Bytes::copy_from_slice(&buffer)).await?;
            }
        }
        Err(e) => {
            error!(
                "[Static file serving][{}] Failed to open {}: {:?}",
                uri, file_path, e
            );
            sender.send_response(build_unfound_response()?).await?;
        }
    }
    Ok(())
}

async fn index(file_path: &mut String, _index: &[String]) -> Result<()> {
    // 处理 index 文件
    // TODO 遍历 Rule::Index 规则中设的 index 文件名，并设置文件路径
    if let Ok(meta) = tokio::fs::metadata(&file_path).await {
        if meta.is_dir() {
            if !file_path.ends_with('/') {
                file_path.push('/');
            }
            file_path.push_str("index.html");
        }
    }
    Ok(())
}

/// TODO 通过 MIME 类型推断文件类型
fn infer_content_type<'a>(
    file_path: &str,
    mime_types: &'a HashMap<String, String>,
    default_type: Option<&'a String>,
) -> Option<&'a String> {
    let ext = file_path.rsplit(".").next()?;

    info!("Infer content type for file: {}", file_path);
    debug!("Extension: {}", ext);

    match mime_types.get(ext) {
        Some(content_type) => Some(content_type),
        None => default_type,
    }
}

fn build_unfound_response() -> Result<Response<()>> {
    Ok(Response::builder()
        .status(404)
        .body(())
        .inspect_err(|e| error!("Failed to build 404 response: {}", e))?)
}

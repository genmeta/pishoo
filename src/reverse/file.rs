use std::collections::HashMap;

use bytes::Bytes;
use h3::server::RequestStream;
use h3_shim::SendStream;
use http::{Request, Response, Uri};
use tokio::io::{AsyncReadExt, BufReader};
use tracing::{debug, error};

use crate::{
    error::{CustomError, Result},
    parse::location::FileLocation,
    reverse::build_error_response,
};

/// 处理 ROOT 静态文件请求
pub async fn root(
    location: &FileLocation,
    req: Request<()>,
    sender: &mut RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    // 对于 Root 类型，把请求路径直接拼接到根目录上
    let mut file_path = format!("{}{}", location.replace, req.uri().path());

    serve_static_file(
        &mut file_path,
        req.uri(),
        &location.mime_types,
        &location.index_files,
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

    serve_static_file(
        &mut file_path,
        req.uri(),
        &location.mime_types,
        &location.index_files,
        &location.default_type,
        sender,
    )
    .await?;
    Ok(())
}

/// 异步加载文件，并以流式方式通过 sender 发送响应体。
async fn serve_static_file(
    file_path: &mut String,
    uri: &Uri,
    mime_types: &HashMap<String, String>,
    index_files: &[String],
    default_type: &Option<String>,
    sender: &mut RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    match index(file_path, index_files).await {
        Ok(()) => {}
        Err(e) => {
            match e {
                CustomError::FileNotFound(_) => {
                    // 如果 index 文件不存在，直接返回 404
                    sender.send_response(build_unfound_response()?).await?;
                    return Ok(());
                }
                _ => {
                    // 其他错误直接返回
                    error!(
                        "[Static file serving][{}] Failed to get metadata: {:?}",
                        uri, e
                    );
                    sender.send_response(build_error_response()?).await?;
                    return Ok(());
                }
            }
        }
    };

    match tokio::fs::File::open(&*file_path).await {
        Ok(file) => {
            // 获取文件元数据，用于确定文件大小
            let metadata = match tokio::fs::metadata(&*file_path).await {
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
            sender.send_response(build_error_response()?).await?;
        }
    }
    Ok(())
}

async fn index(file_path: &mut String, index_files: &[String]) -> Result<()> {
    // 1. 检查初始路径是否存在及其元数据
    let metadata = match tokio::fs::metadata(&*file_path).await {
        Ok(meta) => meta,
        Err(_) => {
            // 如果初始路径本身就不存在，直接返回错误
            return Err(CustomError::FileNotFound(format!(
                "Failed to get metadata for initial path: {}",
                file_path
            )));
        }
    };

    // 2. 检查是否是目录
    if metadata.is_dir() {
        if !file_path.ends_with('/') {
            file_path.push('/');
        }
        let base_dir_path = file_path.clone();

        // 3. 遍历 index_files 列表
        let mut found_index = false;
        for index_filename in index_files {
            // 构建当前尝试的完整文件路径
            let mut potential_path = base_dir_path.clone();
            potential_path.push_str(index_filename);

            // 4. 检查拼接后的文件路径是否存在
            match tokio::fs::metadata(&potential_path).await {
                Ok(index_meta) => {
                    // 确保找到的是文件，而不是子目录或其他类型
                    if index_meta.is_file() {
                        // 找到了存在的文件！更新 file_path 并退出循环
                        *file_path = potential_path;
                        found_index = true;
                        break; // 找到第一个就跳出循环
                    }
                    // 如果存在但不是文件（例如是同名目录），则继续尝试下一个 index 文件
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }

        // 5. 如果循环结束后仍未找到任何 index 文件
        if !found_index {
            return Err(CustomError::FileNotFound(format!(
                "No suitable index file found in directory '{}'",
                base_dir_path
            )));
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
    let ext = match file_path.rsplit(".").next() {
        Some(ext) => ext.to_lowercase(),
        None => return default_type,
    };
    match mime_types.get(&ext) {
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

use std::sync::Arc;

use async_compression::{Level, tokio::bufread::GzipEncoder};
use http::response::Parts;
use tokio_util::io::ReaderStream;

use crate::parse::Node;

/// Gzip 压缩配置，从 location 节点中提取
pub struct GzipConfig {
    pub enabled: bool,
    pub accept_gzip: bool,
    pub vary: bool,
    pub min_length: u64,
    pub comp_level: i32,
    pub types: Option<Vec<String>>,
}

impl GzipConfig {
    /// 从 location 配置和请求头中提取 gzip 配置
    pub fn from_location(location: &Arc<Node>, accept_encoding: Option<&str>) -> Self {
        let accept_gzip = accept_encoding.map(|v| v.contains("gzip")).unwrap_or(false);

        Self {
            enabled: location.get_bool("gzip").unwrap_or(false),
            accept_gzip,
            vary: location.get_bool("gzip_vary").unwrap_or(false),
            min_length: location.get_str_parsed("gzip_min_length").unwrap_or(20),
            comp_level: location.get_str_parsed("gzip_comp_level").unwrap_or(1),
            types: location.get_string_vec("gzip_types"),
        }
    }

    /// 判断是否应该压缩
    pub fn should_compress(&self, parts: &Parts, content_length: Option<u64>) -> bool {
        if !self.enabled || !self.accept_gzip {
            return false;
        }

        if parts.headers.get(http::header::CONTENT_ENCODING).is_some() {
            return false;
        }

        let length_ok = content_length.map(|l| l >= self.min_length).unwrap_or(true);

        let content_type = parts
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or(v).trim())
            .unwrap_or("");

        let type_ok = match &self.types {
            None => content_type == "text/html",
            Some(types) => types.iter().any(|t| t == "*" || t == content_type),
        };

        length_ok && type_ok
    }

    /// 应用 gzip 响应头（Content-Encoding、Vary 等）
    pub fn apply_headers(&self, parts: &mut Parts) {
        parts
            .headers
            .insert(http::header::CONTENT_ENCODING, "gzip".parse().unwrap());
        parts.headers.remove(http::header::CONTENT_LENGTH);

        if self.vary {
            parts
                .headers
                .append(http::header::VARY, "Accept-Encoding".parse().unwrap());
        }
    }

    /// 获取压缩级别
    pub fn level(&self) -> Level {
        match self.comp_level {
            1 => Level::Fastest,
            9 => Level::Best,
            l => Level::Precise(l),
        }
    }
}

/// Compress a hyper `Incoming` response body if the location config requires it.
///
/// Returns a new response with the body wrapped in gzip, or the original response unchanged.
pub fn compress_response(
    location: &Arc<Node>,
    accept_encoding: Option<&str>,
    response: http::Response<hyper::body::Incoming>,
) -> http::Response<axum::body::Body> {
    use futures::TryStreamExt;

    let gzip = GzipConfig::from_location(location, accept_encoding);

    let (mut parts, body) = response.into_parts();

    let content_length = parts
        .headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let should_compress = gzip.should_compress(&parts, content_length);

    if should_compress {
        gzip.apply_headers(&mut parts);

        let body_stream = tokio_util::io::StreamReader::new(
            http_body_util::BodyExt::into_data_stream(body)
                .map_err(std::io::Error::other),
        );
        let compressed = GzipEncoder::with_quality(body_stream, gzip.level());
        let stream = ReaderStream::new(compressed);
        let body = axum::body::Body::from_stream(stream);
        http::Response::from_parts(parts, body)
    } else {
        http::Response::from_parts(parts, axum::body::Body::new(body))
    }
}

/// Compress a file body with the same gzip config logic.
///
/// Returns the response with appropriate headers and optionally compressed body.
pub fn compress_file_response(
    location: &Arc<Node>,
    accept_encoding: Option<&str>,
    mut parts: http::response::Parts,
    file: tokio::fs::File,
    file_size: u64,
) -> http::Response<axum::body::Body> {
    let gzip = GzipConfig::from_location(location, accept_encoding);
    let should_compress = gzip.should_compress(&parts, Some(file_size));

    if should_compress {
        gzip.apply_headers(&mut parts);

        let reader = tokio::io::BufReader::new(file);
        let compressed = GzipEncoder::with_quality(reader, gzip.level());
        let stream = ReaderStream::new(compressed);
        let body = axum::body::Body::from_stream(stream);
        http::Response::from_parts(parts, body)
    } else {
        parts
            .headers
            .insert(http::header::CONTENT_LENGTH, file_size.into());

        let stream = ReaderStream::new(file);
        let body = axum::body::Body::from_stream(stream);
        http::Response::from_parts(parts, body)
    }
}

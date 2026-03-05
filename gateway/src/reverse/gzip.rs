use std::sync::Arc;

use async_compression::{Level, tokio::bufread::GzipEncoder};
use http::response::Parts;

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

    /// 根据是否压缩，包装 reader
    pub fn wrap_reader<R: tokio::io::AsyncBufRead + Unpin + Send + 'static>(
        &self,
        should_compress: bool,
        reader: R,
    ) -> Box<dyn tokio::io::AsyncRead + Unpin + Send> {
        if should_compress {
            Box::new(GzipEncoder::with_quality(reader, self.level()))
        } else {
            Box::new(reader)
        }
    }
}

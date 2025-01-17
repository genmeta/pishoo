use std::{net::IpAddr, str::FromStr};

use bytes::Bytes;
use http::{HeaderMap, Method, Uri, response::Builder};

use crate::error::Result;

pub(super) async fn handler(
    method: &Method,
    uri: &Uri,
    target: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<(Builder, Bytes)> {
    // 构建目标 URI
    let uri = format!(
        "{}{}{}",
        target,
        uri.path(),
        uri.query().map(|q| format!("?{}", q)).unwrap_or_default()
    );

    // 解析目标地址
    let target = Uri::from_str(target)?;
    let domain = target.host().expect("no domain");
    let dns = if IpAddr::from_str(domain).is_err() {
        // TODO 后续支持分布式域名解析
        Some((domain, "127.0.0.1:80".parse().unwrap()))
    } else {
        None
    };

    // 构建请求
    let mut req =
        crate::client::client("127.0.0.1".parse().unwrap(), dns).request(method.clone(), &uri);
    for (key, value) in headers.iter() {
        req = req.header(key, value);
    }
    let response = req.body(body).send().await?;

    // 发送请求并获取响应
    let mut builder = http::Response::builder().status(response.status());
    for (key, value) in response.headers().iter() {
        builder = builder.header(key, value);
    }
    let body = response.bytes().await?;

    Ok((builder, body))
}

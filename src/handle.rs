use std::collections::HashMap;

use axum::{body::Body, response::Response};
use bytes::{Buf, Bytes};
use h3::server::RequestStream;
use h3_shim::BidiStream;
use http::{Method, Request, Uri, response::Builder};
use tracing::info;

use crate::{
    error::{CustomError, Result},
    parse::{router::Router, rule::Rule},
};

mod proxy;
mod static_file;

pub async fn handler_http3(
    routers: &HashMap<String, Router>,
    req: Request<()>,
    mut stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    // 读取请求体
    let mut body = Vec::new();
    while let Some(chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.chunk());
    }

    // 处理请求
    let (builder, body) =
        handle(req.method(), req.uri(), req.headers(), body.into(), routers).await?;
    // 构建响应
    let response = builder.body(())?;
    stream.send_response(response).await?;
    // 发送响应体
    if !body.is_empty() {
        stream.send_data(body).await?;
    }
    stream.finish().await?;

    Ok(())
}

pub async fn handle_http(routers: HashMap<String, Router>, req: Request<Body>) -> Result<Response> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await?;

    let (builder, body) = handle(&method, &uri, &headers, body, &routers).await?;
    let response = builder.body(Body::from(body))?;
    Ok(response)
}

async fn handle(
    method: &Method,
    uri: &Uri,
    headers: &http::HeaderMap,
    body: Bytes,
    routers: &HashMap<String, Router>,
) -> Result<(Builder, Bytes)> {
    info!("method: {}, uri: {}, headers: {:?}", method, uri, headers);

    // 提取主机名
    let host = uri.host().ok_or(CustomError::Unknown)?.to_string();

    info!("host: {}", host);

    // 获取对应的服务器配置
    let router = routers.get(&host).ok_or(CustomError::Unknown)?;

    // 获取路径并匹配路由规则
    let path = uri.path();
    let (pattern, rules) = router.route(path)?;

    // 解析规则
    let mut proxy = None;
    let mut static_file = None;

    for rule in rules {
        match rule {
            Rule::Allow(_) => {
                // TODO: 实现鉴权逻辑
            }
            Rule::Deny(_) => {
                // TODO: 实现鉴权逻辑
            }
            Rule::ProxyPass(target) => proxy = Some(target),
            Rule::Root(path) => static_file = Some(path),
        }
    }

    // 根据规则处理请求
    let (builder, body) = if let Some(target) = proxy {
        info!("proxy to {}", target);
        proxy::handler(method, uri, &target, headers, body).await?
    } else if let Some(root) = static_file {
        static_file::handler(&pattern, &root, uri)
    } else {
        return Err(CustomError::Unknown);
    };

    // TODO 添加 Header 之类的处理

    Ok((builder, body))
}

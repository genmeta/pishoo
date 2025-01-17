use std::collections::HashMap;

use bytes::Bytes;
use h3::server::RequestStream;
use h3_shim::BidiStream;
use http::Request;

use crate::{
    error::{CustomError, Result},
    parse::{rule::Rule, server::Server},
};

mod proxy;
mod static_file;

pub async fn handler(
    servers: &HashMap<String, Server>,
    req: Request<()>,
    mut stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    // 提取主机名
    let host = req.uri().host().ok_or(CustomError::Unknown)?.to_string();

    // 获取对应的服务器配置
    let server = servers.get(&host).ok_or(CustomError::Unknown)?;

    // 获取路径并匹配路由规则
    let path = req.uri().path();
    let (pattern, rules) = server.route(path)?;

    // 解析规则
    let mut proxy_pass = None;
    let mut root = None;

    for rule in rules {
        match rule {
            Rule::Allow(_) => {
                // TODO: 实现鉴权逻辑
            }
            Rule::Deny(_) => {
                // TODO: 实现鉴权逻辑
            }
            Rule::ProxyPass(target) => proxy_pass = Some(target),
            Rule::Root(path) => root = Some(path),
        }
    }

    // 根据规则处理请求
    if let Some(target) = proxy_pass {
        proxy::handler(target, &req, &mut stream).await?;
    } else if let Some(root) = root {
        static_file::handler(pattern, root, &req, &mut stream).await?;
    } else {
        return Err(CustomError::Unknown);
    }

    Ok(())
}

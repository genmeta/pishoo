use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use crate::error::{CustomError, Result};

// TODO 除了 转发, 以及静态文件 之外, 还有什么其他的规则?

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    Allow(Vec<String>),
    Deny(Vec<String>),
    Root(String),
    ProxyPass(String),
}

// TODO # 路径重写
// TODO # 带状态码的返回配置
// TODO # 响应内容替换

pub fn parse_rule(rule: Directive<Nginx>) -> Result<Rule> {
    match rule.name.as_str() {
        "allow" => Ok(Rule::Allow(rule.args)),
        "deny" => Ok(Rule::Deny(rule.args)),
        "root" => rule
            .args
            .first()
            .map(|root| Rule::Root(root.clone()))
            .ok_or_else(|| CustomError::MissingArg("root".to_string())),
        "proxy_pass" => rule
            .args
            .first()
            .map(|target| Rule::ProxyPass(target.clone()))
            .ok_or_else(|| CustomError::MissingArg("proxy_pass".to_string())),
        _ => {
            info!("unknown directive: {}", rule.name);
            Err(CustomError::UnknownDirective(rule.name))
        }
    }
}

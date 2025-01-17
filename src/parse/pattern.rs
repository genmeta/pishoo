use crate::error::{CustomError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// 精确匹配 (=)
    Exact(String),
    /// 前缀匹配 (^~)
    Prefix(String),
    /// 正则表达式匹配 (~)
    Regex(String),
    /// 正则表达式匹配 不区分大小写 (~*)
    CRegex(String),
    /// 普通前缀匹配
    NormalPrefix(String),
    /// 通用匹配 (/)
    Common,
}

impl Pattern {
    pub fn priority(&self) -> usize {
        match self {
            Pattern::Exact(_) => 0,
            Pattern::Prefix(_) => 1,
            Pattern::Regex(_) => 2,
            Pattern::CRegex(_) => 3,
            Pattern::NormalPrefix(_) => 4,
            Pattern::Common => 5,
        }
    }
}

pub fn parse_pattern(args: &[String]) -> Result<Pattern> {
    let pattern = match args {
        [path] if path == "/" => Pattern::Common,
        [path] if path.starts_with("/") => Pattern::NormalPrefix(path.to_string()),
        [symbol, path] => match symbol.as_str() {
            "=" => Pattern::Exact(path.to_string()),
            "^~" => Pattern::Prefix(path.to_string()),
            "~" => Pattern::Regex(path.to_string()),
            "~*" => Pattern::CRegex(path.to_string()),
            _ => {
                return Err(CustomError::UnsupportedConfig(format!(
                    "unsupported location symbol: {}",
                    symbol
                )));
            }
        },
        _ => {
            return Err(CustomError::UnsupportedConfig(
                "The number of location args must be 1 or 2".to_string(),
            ));
        }
    };
    Ok(pattern)
}

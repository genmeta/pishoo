use regex::Regex;

use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub enum Pattern {
    /// 精确匹配 (=)
    Exact(String),
    /// 前缀匹配 (^~)
    Prefix(String),
    /// 正则表达式匹配 (~)
    Regex(Regex),
    /// 正则表达式匹配 不区分大小写 (~*)
    CRegex(Regex),
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

    /// 统一匹配逻辑
    pub fn try_match(&self, path: &str) -> Result<Option<String>> {
        match self {
            Self::Exact(p) => Ok((path == p).then(|| p.clone())),
            Self::Prefix(p) => {
                let regex_str = format!(r"^{}", p);
                let re = Regex::new(&regex_str)?;
                Ok(re.find(path).map(|m| m.as_str().to_string()))
            }
            Self::Regex(re) => Ok(re.find(path).map(|m| m.as_str().to_string())),
            Self::CRegex(re) => Ok(re.find(path).map(|m| m.as_str().to_string())),
            Self::NormalPrefix(p) => Ok(path.starts_with(p).then(|| p.clone())),
            Self::Common => Ok(Some("/".to_string())),
        }
    }
}

// 保留原有 parse_pattern 函数逻辑
pub fn parse_pattern(args: &[String]) -> Result<Pattern> {
    let pattern = match args {
        [pattern] if pattern == "/" => Pattern::Common,
        [pattern] if pattern.starts_with('/') => Pattern::NormalPrefix(pattern.clone()),
        [symbol, pattern] => match symbol.as_str() {
            "=" => Pattern::Exact(pattern.clone()),
            "^~" => Pattern::Prefix(pattern.clone()),
            "~" => Pattern::Regex(Regex::new(pattern)?),
            "~*" => Pattern::CRegex(Regex::new(&format!("(?i){}", pattern))?),
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

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use crate::parse::{
//         location::Location,
//         router::Router,
//         rule::{ReverseRule, Rule},
//     };

//     #[test]
//     fn test_priority_order() {
//         let mut router = Router::default();

//         // 按随机顺序插入
//         router
//             .insert(Location {
//                 pattern: Pattern::Common,
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();
//         router
//             .insert(Location {
//                 pattern: Pattern::Exact("/api".into()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();
//         router
//             .insert(Location {
//                 pattern: Pattern::Prefix("/v1".into()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();

//         // 验证匹配顺序
//         assert!(router.route("/api").is_ok()); // 匹配Exact
//         assert!(router.route("/v1/test").is_ok()); // 匹配Prefix
//         assert!(router.route("/").is_ok()); // 匹配Common
//     }

//     #[test]
//     fn test_regex_patterns() {
//         let mut router = Router::default();

//         // 插入大小写敏感正则
//         router
//             .insert(Location {
//                 pattern: Pattern::Regex(Regex::new(r"\.jpg$").unwrap()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();

//         // 插入大小写不敏感正则
//         router
//             .insert(Location {
//                 pattern: Pattern::CRegex(Regex::new(r"(?i)\.png$").unwrap()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();

//         // 测试大小写敏感匹配
//         assert!(router.route("/image.jpg").is_ok());
//         assert!(router.route("/image.JPG").is_err());

//         // 测试大小写不敏感匹配
//         assert!(router.route("/image.png").is_ok());
//         assert!(router.route("/image.PNG").is_ok());
//     }

//     #[test]
//     fn test_priority_between_patterns() {
//         let mut router = Router::default();

//         // 插入不同优先级的规则
//         router
//             .insert(Location {
//                 pattern: Pattern::NormalPrefix("/static".into()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();

//         router
//             .insert(Location {
//                 pattern: Pattern::Regex(Regex::new(r"\.css$").unwrap()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();

//         // 即使路径同时匹配普通前缀和正则，应该优先匹配正则
//         let result = router.route("/static/style.css");
//         assert!(result.is_ok());
//         assert_eq!(result.unwrap().0, ".css"); // 匹配正则结果
//     }

//     #[test]
//     #[should_panic]
//     fn test_invalid_regex_handling() {
//         // 测试无效正则表达式处理
//         #[allow(clippy::invalid_regex)]
//         let invalid_re = Regex::new(r"[invalid");
//         assert!(invalid_re.is_err());

//         // 测试插入时的错误处理
//         let mut router = Router::default();
//         let result = router.insert(Location {
//             pattern: Pattern::Regex(invalid_re.unwrap()),
//             rule: Rule::Reverse(ReverseRule::default()),
//         });
//         assert!(result.is_err());
//     }

//     #[test]
//     fn test_pattern_precedence() {
//         let mut router = Router::default();

//         // 相同优先级不同顺序插入
//         router
//             .insert(Location {
//                 pattern: Pattern::Regex(Regex::new(r"a").unwrap()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();

//         router
//             .insert(Location {
//                 pattern: Pattern::Regex(Regex::new(r"ab").unwrap()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();

//         // 先插入的规则应该优先匹配
//         assert_eq!(router.route("abc").unwrap().0, "a");
//     }

//     #[test]
//     fn test_edge_cases() {
//         let mut router = Router::default();

//         // 空路径测试
//         router
//             .insert(Location {
//                 pattern: Pattern::Exact("".into()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();
//         assert!(router.route("").is_ok());

//         // 特殊字符测试
//         router
//             .insert(Location {
//                 pattern: Pattern::Regex(Regex::new(r"\d+").unwrap()),
//                 rule: Rule::Reverse(ReverseRule::default()),
//             })
//             .unwrap();
//         assert!(router.route("/123").is_ok());
//         assert!(router.route("/abc").is_err());
//     }
// }

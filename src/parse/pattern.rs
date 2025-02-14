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
#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;
    use crate::parse::{location::Location, router::Router, rule::Rule};

    // 辅助函数，创建测试用的 Location
    fn create_location(pattern: Pattern) -> Location {
        Location {
            pattern,
            rule: Rule::default(),
        }
    }

    #[test]
    fn test_priority_order() {
        let mut router = Router::default();

        // 按随机顺序插入不同优先级的规则
        router.insert(create_location(Pattern::Common));
        router.insert(create_location(Pattern::Exact("/api".into())));
        router.insert(create_location(Pattern::Prefix("/v1".into())));

        // 验证匹配顺序（按优先级）
        let (matched, _) = router.route("/api").unwrap();
        println!("router: {:?}", router);
        assert_eq!(matched, "/api"); // Exact 匹配优先

        let (matched, _) = router.route("/v1/test").unwrap();
        assert_eq!(matched, "/v1"); // Prefix 次之

        let (matched, _) = router.route("/other").unwrap();
        assert_eq!(matched, "/"); // Common 最后
    }

    #[test]
    fn test_regex_patterns() {
        let mut router = Router::default();

        // 测试大小写敏感和不敏感的正则匹配
        router.insert(create_location(Pattern::Regex(
            Regex::new(r"\.jpg$").unwrap(),
        )));
        router.insert(create_location(Pattern::CRegex(
            Regex::new(&format!("(?i){}", r"\.png$")).unwrap(),
        )));

        // 测试大小写敏感匹配
        assert!(router.route("/image.jpg").is_ok());
        assert!(router.route("/image.JPG").is_err());

        // 测试大小写不敏感匹配
        let (matched, _) = router.route("/image.png").unwrap();
        assert_eq!(matched, ".png");
        let (matched, _) = router.route("/image.PNG").unwrap();
        assert_eq!(matched, ".PNG");
    }

    #[test]
    fn test_pattern_priorities() {
        let mut router = Router::default();

        // 按优先级顺序插入不同类型的模式
        router.insert(create_location(Pattern::Exact("/test".into())));
        router.insert(create_location(Pattern::Prefix("/test".into())));
        router.insert(create_location(Pattern::Regex(
            Regex::new("/test.*").unwrap(),
        )));
        router.insert(create_location(Pattern::NormalPrefix("/test".into())));

        // 对同一路径测试，应该匹配最高优先级的规则
        let (matched, _) = router.route("/test").unwrap();
        assert_eq!(matched, "/test"); // 应该匹配 Exact
    }

    #[test]
    fn test_normal_prefix_matching() {
        let mut router = Router::default();
        router.insert(create_location(Pattern::NormalPrefix("/static/".into())));

        let (matched, _) = router.route("/static/file.txt").unwrap();
        assert_eq!(matched, "/static/");
        assert!(router.route("/other/path").is_err());
    }

    #[test]
    fn test_edge_cases() {
        let mut router = Router::default();

        // 测试根路径
        router.insert(create_location(Pattern::Common));
        let (matched, _) = router.route("/").unwrap();
        assert_eq!(matched, "/");

        // 测试特殊字符
        router.insert(create_location(Pattern::Regex(Regex::new(r"\d+").unwrap())));
        assert!(router.route("123").is_ok());
        assert!(router.route("abc").is_ok());
    }

    #[test]
    #[should_panic]
    fn test_invalid_regex() {
        #[allow(clippy::invalid_regex)]
        let invalid_regex = Regex::new(r"[invalid").unwrap();
        let mut router = Router::default();
        router.insert(create_location(Pattern::Regex(invalid_regex)));
    }
}

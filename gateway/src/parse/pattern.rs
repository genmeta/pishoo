//! URI pattern matching implementation
//!
//! Defines different URL matching patterns and their priority rules
//! based on nginx location matching semantics

use regex::Regex;
use snafu::ResultExt;

use crate::error::Result;

#[derive(Debug, Clone)]
pub enum Pattern {
    /// 精确匹配 (=)
    Exact(String),
    /// 字面量前缀匹配 (^~)
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
            Pattern::Exact(_) => 4,
            Pattern::Prefix(_) => 3,
            Pattern::Regex(_) | Pattern::CRegex(_) => 2,
            Pattern::NormalPrefix(_) => 1,
            Pattern::Common => 0,
        }
    }

    /// 统一匹配逻辑
    pub fn try_match<'s>(&'s self, path: &'s str) -> Result<Option<&'s str>> {
        match self {
            Self::Exact(p) => Ok((path == p).then_some(p.as_str())),
            Self::Prefix(p) => Ok(path.starts_with(p).then_some(p.as_str())),
            Self::Regex(re) => Ok(re.find(path).map(|m| m.as_str())),
            Self::CRegex(re) => Ok(re.find(path).map(|m| m.as_str())),
            Self::NormalPrefix(p) => Ok(path.starts_with(p).then_some(p.as_str())),
            Self::Common => Ok(Some("/")),
        }
    }
}

#[derive(snafu::Snafu, Debug)]
pub enum ParsePatternError {
    #[snafu(display("Unsupported location symbol `{symbol}`"))]
    UnsupportedSymbol { symbol: String },
    #[snafu(display("Invalid regex `{pattern}`"))]
    RegexError {
        source: regex::Error,
        pattern: String,
    },
    #[snafu(display("Number of location args must be 1 or 2, got {nargs}"))]
    UnexpectedArgs { nargs: usize },
}

pub fn parse_pattern(args: &[String]) -> Result<Pattern, ParsePatternError> {
    let pattern = match args {
        [pattern] if pattern == "/" => Pattern::Common,
        [pattern] if pattern.starts_with('/') => Pattern::NormalPrefix(pattern.clone()),
        [symbol, pattern] => match symbol.as_str() {
            "=" => Pattern::Exact(pattern.clone()),
            "^~" => Pattern::Prefix(pattern.clone()),
            "~" => Pattern::Regex(Regex::new(pattern).context(RegexSnafu { pattern })?),
            "~*" => {
                let regex = format!("(?i){pattern}");
                Pattern::CRegex(Regex::new(&regex).context(RegexSnafu { pattern: regex })?)
            }
            symbol => return UnsupportedSymbolSnafu { symbol }.fail(),
        },
        args => return UnexpectedArgsSnafu { nargs: args.len() }.fail(),
    };
    Ok(pattern)
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;

    #[test]
    fn test_try_match_exact() {
        let pattern = Pattern::Exact("/login".to_string());
        assert_eq!(
            pattern.try_match("/login").unwrap(),
            Some("/login"),
            "Exact match positive"
        );
        assert_eq!(
            pattern.try_match("/login/").unwrap(),
            None,
            "Exact match negative (extra slash)"
        );
        assert_eq!(
            pattern.try_match("/logi").unwrap(),
            None,
            "Exact match negative (shorter)"
        );
        assert_eq!(
            pattern.try_match("/Login").unwrap(),
            None,
            "Exact match negative (case)"
        );
        println!("✅ Exact match tests passed!");
    }
    #[test]
    fn test_try_match_prefix() {
        // Prefix Regex is created with '^' in parse_pattern
        let pattern = Pattern::Prefix("/api/v1".to_string());
        assert_eq!(
            pattern.try_match("/api/v1/users").unwrap(),
            Some("/api/v1"),
            "Prefix match positive"
        );
        assert_eq!(
            pattern.try_match("/api/v1").unwrap(),
            Some("/api/v1"),
            "Prefix match exact"
        );
        assert_eq!(
            pattern.try_match("/api/v2").unwrap(),
            None,
            "Prefix match negative (different)"
        );
        assert_eq!(
            pattern.try_match("/API/v1").unwrap(),
            None,
            "Prefix match negative (case)"
        );
        assert_eq!(
            pattern.try_match("/other/api/v1").unwrap(),
            None,
            "Prefix match negative (not at start)"
        );
        println!("✅ Prefix match tests passed!");
    }

    #[test]
    fn test_try_match_regex() {
        let pattern = Pattern::Regex(Regex::new(r"\.(jpg|gif)$").unwrap());
        assert_eq!(
            pattern.try_match("/images/cat.jpg").unwrap(),
            Some(".jpg"),
            "Regex match positive (jpg)"
        );
        assert_eq!(
            pattern.try_match("/images/dog.gif").unwrap(),
            Some(".gif"),
            "Regex match positive (gif)"
        );
        assert_eq!(
            pattern.try_match("/images/cat.JPG").unwrap(),
            None,
            "Regex match negative (case)"
        );
        assert_eq!(
            pattern.try_match("/images/cat.png").unwrap(),
            None,
            "Regex match negative (wrong extension)"
        );
        assert_eq!(
            pattern.try_match("cat.jpg/images/").unwrap(),
            None,
            "Regex match negative (not at end)"
        );
        println!("✅ Regex match tests passed!");
    }

    #[test]
    fn test_try_match_cregex() {
        // CRegex Regex is created with '(?i)' in parse_pattern
        let pattern = Pattern::CRegex(Regex::new(r"(?i)\.(jpg|gif)$").unwrap());
        assert_eq!(
            pattern.try_match("/images/cat.jpg").unwrap(),
            Some(".jpg"),
            "CRegex match positive (jpg)"
        );
        assert_eq!(
            pattern.try_match("/images/cat.JPG").unwrap(),
            Some(".JPG"),
            "CRegex match positive (JPG)"
        );
        assert_eq!(
            pattern.try_match("/images/DOG.gif").unwrap(),
            Some(".gif"),
            "CRegex match positive (DOG.gif)"
        );
        assert_eq!(
            pattern.try_match("/images/cat.png").unwrap(),
            None,
            "CRegex match negative (wrong extension)"
        );
        assert_eq!(
            pattern.try_match("cat.jpg/images/").unwrap(),
            None,
            "CRegex match negative (not at end)"
        );
        println!("✅ Case-insensitive Regex match tests passed!");
    }

    #[test]
    fn test_try_match_normal_prefix() {
        let pattern = Pattern::NormalPrefix("/static/".to_string());
        assert_eq!(
            pattern.try_match("/static/css/style.css").unwrap(),
            Some("/static/"),
            "NormalPrefix match positive"
        );
        assert_eq!(
            pattern.try_match("/static/").unwrap(),
            Some("/static/"),
            "NormalPrefix match exact"
        );
        assert_eq!(
            pattern.try_match("/static").unwrap(),
            None,
            "NormalPrefix match negative (no trailing slash)"
        );
        assert_eq!(
            pattern.try_match("/Static/js/app.js").unwrap(),
            None,
            "NormalPrefix match negative (case)"
        );
        assert_eq!(
            pattern.try_match("/other/static/").unwrap(),
            None,
            "NormalPrefix match negative (not at start)"
        );
        println!("✅ NormalPrefix match tests passed!");
    }

    #[test]
    fn test_try_match_common() {
        let pattern = Pattern::Common;
        assert_eq!(
            pattern.try_match("/anything").unwrap(),
            Some("/"),
            "Common match any path"
        );
        assert_eq!(
            pattern.try_match("/").unwrap(),
            Some("/"),
            "Common match root path"
        );
        assert_eq!(
            pattern.try_match("").unwrap(),
            Some("/"),
            "Common match empty path"
        ); // Assuming Common should match empty path too
        println!("✅ Common match tests passed!");
    }

    #[test]
    fn test_parse_pattern_invalid_symbol() {
        let args = vec!["??".to_string(), "/path".to_string()];
        let result = parse_pattern(&args);
        assert!(matches!(
            result,
            Err(ParsePatternError::UnsupportedSymbol { .. })
        ));
        println!("✅ Invalid symbol parsing test passed!");
    }

    #[test]
    fn test_parse_pattern_invalid_args_count() {
        let args0: Vec<String> = vec![];
        let result0 = parse_pattern(&args0);
        assert!(matches!(
            result0,
            Err(ParsePatternError::UnexpectedArgs { .. })
        ));
        let args3 = vec!["~".to_string(), "pattern".to_string(), "extra".to_string()];
        let result3 = parse_pattern(&args3);
        assert!(matches!(
            result3,
            Err(ParsePatternError::UnexpectedArgs { .. })
        ));
        println!("✅ Invalid argument count parsing test passed!");
    }

    #[test]
    fn test_parse_pattern_invalid_regex() {
        // Test invalid regex for '~'
        let args_regex = vec!["~".to_string(), "[invalid".to_string()];
        let result_regex = parse_pattern(&args_regex);
        assert!(
            matches!(result_regex, Err(ParsePatternError::RegexError { .. })),
            "Expected RegexError for ~"
        );
        // Test invalid regex for '~*'
        let args_cregex = vec!["~*".to_string(), "(?invalid".to_string()];
        let result_cregex = parse_pattern(&args_cregex);
        assert!(
            matches!(result_cregex, Err(ParsePatternError::RegexError { .. })),
            "Expected RegexError for ~*"
        );
        println!("✅ Invalid regex parsing tests passed!");
    }

    #[derive(Debug)]
    struct MockNode {
        value: MockValue,
    }
    #[derive(Debug)]
    enum MockValue {
        Pattern(Pattern, String), // String represents some mock location data
    }
    impl MockNode {
        fn value(&self) -> &MockValue {
            &self.value
        }
    }
    // match_location 的独立版本，用于测试，避免依赖外部 crate 结构
    fn match_location_test<'l: 's, 's>(
        locations: &'l [MockNode],
        path: &'s str,
    ) -> Option<(&'l MockNode, &'s str)> {
        let mut location_matched = None;
        let mut pattern_level = 0; // 注意：初始值应小于最低优先级
        let mut matched_len = 0;
        let mut final_pattern = "";
        let mut found_match_at_level = false; // 标记是否在当前最高 level 找到过匹配
        for location in locations {
            let MockValue::Pattern(pattern, _) = location.value();
            let current_priority = pattern.priority();
            // 1. 如果当前模式优先级低于已找到的最高优先级，跳过
            if found_match_at_level && current_priority < pattern_level {
                continue;
            }
            if let Ok(Some(matched)) = pattern.try_match(path) {
                // 2. 如果找到了更高优先级的匹配，直接更新
                if current_priority > pattern_level {
                    location_matched = Some(location);
                    pattern_level = current_priority;
                    matched_len = matched.len();
                    final_pattern = matched;
                    found_match_at_level = true; // 标记找到匹配
                }
                // 3. 如果优先级相同，选择匹配长度更长的
                else if current_priority == pattern_level && matched.len() > matched_len {
                    location_matched = Some(location);
                    // pattern_level 不变
                    matched_len = matched.len();
                    final_pattern = matched;
                    // found_match_at_level 保持 true
                }
                // 4. 如果优先级相同且长度相同，第一个遇到的优先 (这个逻辑在此实现中由遍历顺序保证)
                else if current_priority == pattern_level && !found_match_at_level {
                    // 处理第一次匹配到某个优先级的情况
                    location_matched = Some(location);
                    pattern_level = current_priority;
                    matched_len = matched.len();
                    final_pattern = matched;
                    found_match_at_level = true;
                }
            }
        }
        location_matched.map(move |loc| (loc, final_pattern))
    }

    #[test]
    fn test_match_location_priority_and_length() {
        let locations = vec![
            // Common - Pri 0
            MockNode {
                value: MockValue::Pattern(Pattern::Common, "common_data".into()),
            },
            // NormalPrefix - Pri 1
            MockNode {
                value: MockValue::Pattern(
                    Pattern::NormalPrefix("/images/".into()),
                    "normal_prefix_images_data".into(),
                ),
            },
            // NormalPrefix longer - Pri 1
            MockNode {
                value: MockValue::Pattern(
                    Pattern::NormalPrefix("/images/icons/".into()),
                    "normal_prefix_icons_data".into(),
                ),
            },
            // Regex - Pri 3
            MockNode {
                value: MockValue::Pattern(
                    Pattern::Regex(Regex::new(r"\.png$").unwrap()),
                    "regex_png_data".into(),
                ),
            },
            // CRegex - Pri 2
            MockNode {
                value: MockValue::Pattern(
                    Pattern::CRegex(Regex::new(r"(?i)/Images/icons/.*").unwrap()),
                    "cregex_images_icons_data".into(),
                ),
            },
            // Prefix - Pri 4
            MockNode {
                value: MockValue::Pattern(
                    Pattern::Prefix("/images".to_string()),
                    "prefix_images_data".into(),
                ),
            },
            // Exact - Pri 5
            MockNode {
                value: MockValue::Pattern(
                    Pattern::Exact("/images/icons/logo.png".into()),
                    "exact_logo_data".into(),
                ),
            },
        ];
        // 1. Test Exact match (Highest priority)
        let (matched_node, final_pattern) =
            match_location_test(&locations, "/images/icons/logo.png").unwrap();
        if let MockValue::Pattern(Pattern::Exact(_), data) = matched_node.value() {
            assert_eq!(data, "exact_logo_data");
            assert_eq!(final_pattern, "/images/icons/logo.png");
        } else {
            panic!("Expected Exact match");
        }
        println!("✅ match_location: Exact wins");
        // 2. Test Prefix match (Second highest)
        let (matched_node, final_pattern) =
            match_location_test(&locations, "/images/other.jpg").unwrap();
        if let MockValue::Pattern(Pattern::Prefix(_), data) = matched_node.value() {
            assert_eq!(data, "prefix_images_data");
            assert_eq!(final_pattern, "/images"); // Regex matched "/images"
        } else {
            panic!("Expected Prefix match");
        }
        println!("✅ match_location: Prefix wins over lower priorities");
        // 3. Test Regex match (Higher than CRegex and NormalPrefix)
        let (matched_node, final_pattern) =
            match_location_test(&locations, "/data/file.png").unwrap();
        if let MockValue::Pattern(Pattern::Regex(_), data) = matched_node.value() {
            assert_eq!(data, "regex_png_data");
            assert_eq!(final_pattern, ".png"); // Regex matched ".png"
        } else {
            panic!("Expected Regex match");
        }
        println!("✅ match_location: Regex wins over CRegex/NormalPrefix/Common");
        // 4. Test CRegex match (Higher than NormalPrefix) - Path matches CRegex but not higher Regex
        let (matched_node, final_pattern) =
            match_location_test(&locations, "/Images/icons/avatar.svg").unwrap(); // Case insensitive /Images/icons/
        if let MockValue::Pattern(Pattern::CRegex(_), data) = matched_node.value() {
            assert_eq!(data, "cregex_images_icons_data");
            assert_eq!(final_pattern, "/Images/icons/avatar.svg"); // CRegex matched the whole path part
        } else {
            panic!("Expected CRegex match");
        }
        println!("✅ match_location: CRegex wins over NormalPrefix");
        // 5. Test Normal Prefix - Longest match wins at same priority level
        // This path matches: Prefix (^/images), CRegex ((?i)/Images/icons/.*), NormalPrefix (/images/), NormalPrefix (/images/icons/), Common (/)
        // Highest priority is Prefix (Pri 4).
        // But wait, the original nginx logic for "^~" stops regex matching. Let's re-evaluate based on the provided `match_location`.
        // The provided `match_location` doesn't explicitly stop regex checks after prefix match. It picks the highest priority first.
        // Priority Order: Exact(5) > Prefix(4) > Regex(3) > CRegex(2) > NormalPrefix(1) > Common(0)
        // Path: "/images/icons/favicon.ico"
        // Matches:
        // - Common: Yes (Pri 0, Match "/")
        // - NormalPrefix("/images/"): Yes (Pri 1, Match "/images/")
        // - NormalPrefix("/images/icons/"): Yes (Pri 1, Match "/images/icons/") -> Wins over shorter NormalPrefix
        // - CRegex("(?i)/Images/icons/.*"): Yes (Pri 2, Match "/images/icons/favicon.ico") -> Wins over NormalPrefixes
        // - Regex("\.png$"): No
        // - Prefix("^/images"): Yes (Pri 4, Match "/images") -> Wins over CRegex
        // - Exact: No
        // So, Prefix should win.
        let (matched_node, final_pattern) =
            match_location_test(&locations, "/images/icons/favicon.ico").unwrap();
        if let MockValue::Pattern(Pattern::Prefix(_), data) = matched_node.value() {
            assert_eq!(data, "prefix_images_data");
            assert_eq!(final_pattern, "/images");
        } else {
            panic!("Expected Prefix match due to higher priority");
        }
        println!(
            "✅ match_location: Prefix (/images) wins for /images/icons/favicon.ico due to priority"
        );
        // Test Normal Prefix - Longest match wins (when only NormalPrefix and Common match)
        // Matches:
        // - Common: Yes (Pri 0, Match "/")
        // - NormalPrefix("/images/"): Yes (Pri 1, Match "/images/")
        // - NormalPrefix("/images/icons/"): Yes (Pri 1, Match "/images/icons/") -> Wins over shorter NormalPrefix
        // - CRegex("(?i)/Images/icons/.*"): Yes (Pri 2, Match "/images/icons/extra/stuff") -> Wins over NormalPrefix
        // - Regex("\.png$"): No
        // - Prefix("^/images"): Yes (Pri 4, Match "/images") -> Wins!
        let (matched_node, final_pattern) =
            match_location_test(&locations, "/images/icons/extra/stuff").unwrap();
        if let MockValue::Pattern(Pattern::Prefix(_), data) = matched_node.value() {
            assert_eq!(data, "prefix_images_data");
            assert_eq!(final_pattern, "/images");
        } else {
            panic!("Expected Prefix match due to higher priority");
        }
        println!(
            "✅ match_location: Prefix (/images) wins for /images/icons/extra/stuff due to priority"
        );
        // Test path matching ONLY NormalPrefix and Common
        // Matches:
        // - Common: Yes (Pri 0, Match "/")
        // - NormalPrefix("/images/"): Yes (Pri 1, Match "/images/") -> Wins over Common
        // - CRegex: No
        // - Regex: No
        // - Prefix: Yes (Pri 4, Match "/images") -> Wins!
        let (matched_node, final_pattern) =
            match_location_test(&locations, "/images/other_stuff").unwrap();
        if let MockValue::Pattern(Pattern::Prefix(_), data) = matched_node.value() {
            assert_eq!(data, "prefix_images_data");
            assert_eq!(final_pattern, "/images");
        } else {
            panic!("Expected Prefix match due to higher priority");
        }
        println!("✅ match_location: Prefix (/images) wins for /images/other_stuff");
        // 6. Test Common match (Lowest priority)
        let (matched_node, final_pattern) =
            match_location_test(&locations, "/unmatched/path").unwrap();
        if let MockValue::Pattern(Pattern::Common, data) = matched_node.value() {
            assert_eq!(data, "common_data");
            assert_eq!(final_pattern, "/");
        } else {
            panic!("Expected Common match");
        }
        println!("✅ match_location: Common wins when nothing else matches");
        // 7. Test No Match (if Common wasn't present)
        let locations_no_common = vec![
            MockNode {
                value: MockValue::Pattern(
                    Pattern::NormalPrefix("/images/".into()),
                    "normal_prefix_images_data".into(),
                ),
            },
            MockNode {
                value: MockValue::Pattern(
                    Pattern::Regex(Regex::new(r"\.png$").unwrap()),
                    "regex_png_data".into(),
                ),
            },
            MockNode {
                value: MockValue::Pattern(
                    Pattern::Exact("/login".into()),
                    "exact_login_data".into(),
                ),
            },
        ];
        assert!(match_location_test(&locations_no_common, "/unmatched/path").is_none());
        println!("✅ match_location: No match when applicable");
    }
}

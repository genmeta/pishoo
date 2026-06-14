use std::sync::Arc;

use crate::parse::{document::ConfigNode, pattern::Pattern};

/// Result of matching a request path against configured location blocks.
///
/// Replaces the old `final_pattern` string, providing clear semantics:
/// - `matched`: the portion of the pattern that was matched
/// - `remaining`: the path suffix after stripping the matched prefix
#[derive(Clone, Debug)]
pub struct LocationMatch {
    /// The matched location configuration node.
    pub location: Arc<ConfigNode>,
    /// The portion of the path that was matched by the pattern.
    ///
    /// For prefix patterns, this is the pattern string itself (e.g. "/ssh/").
    /// For regex patterns, this is the regex match result (e.g. ".jpg").
    pub matched: String,
    /// The remaining path after stripping the matched prefix.
    ///
    /// For prefix-style patterns (`Prefix`, `NormalPrefix`, `Exact`, `Common`),
    /// this is `path[matched.len()..]`.
    /// For regex patterns, this equals the full path (regex match positions
    /// are not necessarily prefixes).
    pub remaining: String,
}

impl LocationMatch {
    /// Extract the `Pattern` from the location node.
    pub fn pattern(&self) -> Pattern {
        self.location
            .payload::<Pattern>()
            .expect("location payload type should be a pattern")
            .expect("location node should contain a pattern payload")
            .as_ref()
            .clone()
    }
}

/// Match a request path against all configured location blocks, following
/// nginx's location selection order.
pub fn match_location(locations: &[Arc<ConfigNode>], path: &str) -> Option<LocationMatch> {
    tracing::debug!("all locations {:#?}, path: {:?}", locations, path);

    let mut exact: Option<LocationMatch> = None;
    let mut longest_prefix: Option<LocationMatch> = None;
    let mut longest_prefix_is_caret = false;
    let mut regex_locations = Vec::new();

    for location in locations {
        let pattern = location
            .payload::<Pattern>()
            .ok()
            .flatten()
            .expect("location node should contain a pattern payload");

        match pattern.as_ref() {
            Pattern::Exact(expected) if path == expected => {
                exact = Some(build_location_match(location, expected.clone(), path));
                break;
            }
            Pattern::Prefix(prefix) if path.starts_with(prefix) => {
                let candidate = build_location_match(location, prefix.clone(), path);
                if longest_prefix
                    .as_ref()
                    .is_none_or(|current| candidate.matched.len() > current.matched.len())
                {
                    longest_prefix = Some(candidate);
                    longest_prefix_is_caret = true;
                }
            }
            Pattern::NormalPrefix(prefix) if path.starts_with(prefix) => {
                let candidate = build_location_match(location, prefix.clone(), path);
                if longest_prefix
                    .as_ref()
                    .is_none_or(|current| candidate.matched.len() > current.matched.len())
                {
                    longest_prefix = Some(candidate);
                    longest_prefix_is_caret = false;
                }
            }
            Pattern::Regex(_) | Pattern::CRegex(_) => regex_locations.push(Arc::clone(location)),
            Pattern::Common if longest_prefix.is_none() => {
                longest_prefix = Some(build_location_match(location, "/".to_string(), path));
                longest_prefix_is_caret = false;
            }
            _ => {}
        }
    }

    if let Some(exact) = exact {
        return Some(exact);
    }
    if longest_prefix_is_caret {
        return longest_prefix;
    }
    for location in regex_locations {
        let pattern = location
            .payload::<Pattern>()
            .ok()
            .flatten()
            .expect("location node should contain a pattern payload");
        if let Ok(Some(matched)) = pattern.try_match(path) {
            return Some(build_location_match(&location, matched.to_owned(), path));
        }
    }
    longest_prefix
}

fn build_location_match(location: &Arc<ConfigNode>, matched: String, path: &str) -> LocationMatch {
    let remaining = compute_remaining(location, path, &matched);
    LocationMatch {
        location: Arc::clone(location),
        matched,
        remaining,
    }
}

/// Compute the remaining path suffix after the matched portion.
fn compute_remaining(location: &ConfigNode, path: &str, matched: &str) -> String {
    let pattern = location
        .payload::<Pattern>()
        .expect("location payload type should be a pattern")
        .expect("location node should contain a pattern payload");

    match pattern.as_ref() {
        // For prefix-style patterns, the matched string IS the prefix — strip it.
        Pattern::Exact(_) | Pattern::Prefix(_) | Pattern::NormalPrefix(_) | Pattern::Common => {
            path.strip_prefix(matched).unwrap_or("").to_string()
        }
        // For regex patterns, the match position is arbitrary (not necessarily a prefix),
        // so we return the full path as remaining.
        Pattern::Regex(_) | Pattern::CRegex(_) => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;
    use crate::parse::{
        registry::context,
        source::{SourceId, SourceSpan},
        value::TypedValue,
    };

    fn make_location(pattern: Pattern) -> Arc<ConfigNode> {
        let span = SourceSpan::new(SourceId(0), 0, 0);
        let mut node = ConfigNode::new(context::LOCATION, None, span);
        node.set_payload(TypedValue::new(pattern, span));
        Arc::new(node)
    }

    #[test]
    fn test_normal_prefix_remaining() {
        let locations = vec![make_location(Pattern::NormalPrefix("/ssh/".to_string()))];

        let m = match_location(&locations, "/ssh/yiyue").unwrap();
        assert_eq!(m.matched, "/ssh/");
        assert_eq!(m.remaining, "yiyue");
    }

    #[test]
    fn test_exact_match_remaining() {
        let locations = vec![make_location(Pattern::Exact("/login".to_string()))];

        let m = match_location(&locations, "/login").unwrap();
        assert_eq!(m.matched, "/login");
        assert_eq!(m.remaining, "");
    }

    #[test]
    fn test_common_match_remaining() {
        let locations = vec![make_location(Pattern::Common)];

        let m = match_location(&locations, "/any/path").unwrap();
        assert_eq!(m.matched, "/");
        assert_eq!(m.remaining, "any/path");
    }

    #[test]
    fn test_regex_match_remaining_is_full_path() {
        let locations = vec![make_location(Pattern::Regex(
            Regex::new(r"\.(jpg|gif)$").unwrap(),
        ))];

        let m = match_location(&locations, "/images/cat.jpg").unwrap();
        assert_eq!(m.matched, ".jpg");
        assert_eq!(m.remaining, "/images/cat.jpg");
    }

    #[test]
    fn test_priority_exact_over_prefix() {
        let locations = vec![
            make_location(Pattern::NormalPrefix("/api".to_string())),
            make_location(Pattern::Exact("/api".to_string())),
        ];

        let m = match_location(&locations, "/api").unwrap();
        assert_eq!(m.matched, "/api");
        // Exact has higher priority
        assert!(matches!(m.pattern(), Pattern::Exact(_)));
    }

    #[test]
    fn test_no_match_returns_none() {
        let locations = vec![make_location(Pattern::Exact("/login".to_string()))];

        assert!(match_location(&locations, "/register").is_none());
    }

    #[test]
    fn test_longest_match_same_priority() {
        let locations = vec![
            make_location(Pattern::NormalPrefix("/api".to_string())),
            make_location(Pattern::NormalPrefix("/api/v1".to_string())),
        ];

        let m = match_location(&locations, "/api/v1/users").unwrap();
        assert_eq!(m.matched, "/api/v1");
        assert_eq!(m.remaining, "/users");
    }

    #[test]
    fn test_regex_first_match_beats_normal_prefix_when_no_caret_prefix_exists() {
        let locations = vec![
            make_location(Pattern::NormalPrefix("/app/".to_string())),
            make_location(Pattern::Regex(regex::Regex::new(r"\.php$").unwrap())),
            make_location(Pattern::Regex(regex::Regex::new(r"app/.+").unwrap())),
        ];

        let m = match_location(&locations, "/app/index.php").unwrap();
        assert!(matches!(m.pattern(), Pattern::Regex(_)));
        assert_eq!(m.matched, ".php");
    }

    #[test]
    fn test_caret_prefix_blocks_regex() {
        let locations = vec![
            make_location(Pattern::Prefix("/app/".to_string())),
            make_location(Pattern::Regex(regex::Regex::new(r"\.php$").unwrap())),
        ];

        let m = match_location(&locations, "/app/index.php").unwrap();
        assert!(matches!(m.pattern(), Pattern::Prefix(_)));
        assert_eq!(m.matched, "/app/");
    }

    #[test]
    fn test_root_location_is_prefix_fallback() {
        let locations = vec![
            make_location(Pattern::Common),
            make_location(Pattern::NormalPrefix("/api/".to_string())),
        ];

        let m = match_location(&locations, "/plain").unwrap();
        assert!(matches!(m.pattern(), Pattern::Common));
        assert_eq!(m.matched, "/");
    }
}

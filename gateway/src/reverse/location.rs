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
/// nginx's priority and longest-match semantics.
///
/// Priority order: Exact (4) > Prefix (3) > Regex/CRegex (2) > NormalPrefix (1) > Common (0).
/// Within the same priority level, the longest match wins.
///
/// Returns `None` if no location matches.
pub fn match_location(locations: &[Arc<ConfigNode>], path: &str) -> Option<LocationMatch> {
    tracing::debug!("all locations {:#?}, path: {:?}", locations, path);

    let mut best: Option<(&Arc<ConfigNode>, String, usize)> = None; // (node, matched_str, priority)

    for location in locations {
        let pattern = location
            .payload::<Pattern>()
            .ok()
            .flatten()
            .expect("location node should contain a pattern payload");

        let priority = pattern.priority();

        // Skip if this pattern's priority is lower than what we already have
        if let Some((_, _, best_priority)) = &best
            && priority < *best_priority
        {
            continue;
        }

        if let Ok(Some(matched)) = pattern.try_match(path) {
            let dominated = best.as_ref().is_none_or(|(_, best_matched, best_p)| {
                priority > *best_p || (priority == *best_p && matched.len() >= best_matched.len())
            });

            if dominated {
                best = Some((location, matched.to_owned(), priority));
            }
        }
    }

    let (location, matched, _) = best?;

    let remaining = compute_remaining(location, path, &matched);

    Some(LocationMatch {
        location: Arc::clone(location),
        matched,
        remaining,
    })
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
}

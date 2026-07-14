use std::borrow::Cow;

use http::{Request, Uri, header};
use snafu::Snafu;

use crate::{
    parse::{pattern::Pattern, types::ProxyPass},
    reverse::location::LocationMatch,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedRequestUri {
    pub path: String,
    pub query: Option<String>,
}

#[derive(Debug, Snafu)]
pub enum NormalizeRequestUriError {
    #[snafu(display("request path must start with `/`"))]
    RelativePath,
}

pub fn normalize_request_uri(uri: &Uri) -> Result<NormalizedRequestUri, NormalizeRequestUriError> {
    let raw = uri.path();
    if !raw.starts_with('/') {
        return RelativePathSnafu.fail();
    }

    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map(Cow::into_owned)
        .unwrap_or_else(|_| {
            percent_encoding::percent_decode_str(raw)
                .decode_utf8_lossy()
                .into_owned()
        });
    let collapsed = collapse_slashes(&decoded);
    let path = normalize_segments(&collapsed);
    let query = uri.query().map(str::to_owned);

    Ok(NormalizedRequestUri { path, query })
}

pub fn build_upstream_request_target(
    proxy_pass: &ProxyPass,
    loc: &LocationMatch,
    normalized: &NormalizedRequestUri,
) -> Result<String, NormalizeRequestUriError> {
    let path = if let Some(explicit) = proxy_pass.explicit_path_and_query() {
        match loc.pattern() {
            Pattern::Exact(_) | Pattern::Prefix(_) | Pattern::NormalPrefix(_) | Pattern::Common => {
                rewrite_with_explicit_path(explicit, &loc.remaining)
            }
            Pattern::Regex(_) | Pattern::CRegex(_) => normalized.path.clone(),
        }
    } else {
        normalized.path.clone()
    };

    Ok(append_query(path, normalized.query.as_deref()))
}

pub fn build_prefix_slash_redirect(
    prefix: &str,
    normalized: &NormalizedRequestUri,
    public_origin: Option<&str>,
) -> Option<String> {
    let trimmed = prefix.strip_suffix('/')?;
    if normalized.path != trimmed {
        return None;
    }

    let target = append_query(prefix.to_owned(), normalized.query.as_deref());
    Some(prepend_public_origin(public_origin, target))
}

pub fn request_public_origin<B>(request: &Request<B>) -> Option<String> {
    let authority = request
        .uri()
        .authority()
        .map(|authority| authority.as_str())
        .or_else(|| {
            request
                .headers()
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
        })?;
    let scheme = request.uri().scheme_str().unwrap_or("https");
    Some(format!("{scheme}://{authority}"))
}

fn collapse_slashes(path: &str) -> Cow<'_, str> {
    if !path.contains("//") {
        return Cow::Borrowed(path);
    }

    let mut collapsed = String::with_capacity(path.len());
    let mut previous_was_slash = false;

    for ch in path.chars() {
        if ch == '/' {
            if previous_was_slash {
                continue;
            }
            previous_was_slash = true;
        } else {
            previous_was_slash = false;
        }
        collapsed.push(ch);
    }

    Cow::Owned(collapsed)
}

fn normalize_segments(path: &str) -> String {
    let preserve_trailing_slash =
        path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");

    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }

    let mut normalized = String::from("/");
    normalized.push_str(&segments.join("/"));
    if preserve_trailing_slash && normalized != "/" && !normalized.ends_with('/') {
        normalized.push('/');
    }

    normalized
}

fn rewrite_with_explicit_path(explicit: &str, remaining: &str) -> String {
    let explicit_path = explicit.split('?').next().unwrap_or("/");
    let mut rewritten = format!(
        "{}{}",
        explicit_path.trim_end_matches('/'),
        ensure_leading_slash(remaining)
    );
    if explicit_path.ends_with('/') && remaining.is_empty() {
        rewritten.push('/');
    }
    rewritten
}

fn ensure_leading_slash(segment: &str) -> Cow<'_, str> {
    if segment.is_empty() {
        Cow::Borrowed("")
    } else if segment.starts_with('/') {
        Cow::Borrowed(segment)
    } else {
        Cow::Owned(format!("/{segment}"))
    }
}

fn append_query(path: String, query: Option<&str>) -> String {
    match query {
        Some(query) => format!("{path}?{query}"),
        None => path,
    }
}

fn prepend_public_origin(public_origin: Option<&str>, target: String) -> String {
    match public_origin {
        Some(public_origin) => format!("{public_origin}{target}"),
        None => target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parse::{pattern::Pattern, tests::parse_location_pattern, types::ProxyPass},
        reverse::location::LocationMatch,
    };

    fn make_location_match(pattern: Pattern, matched: &str, remaining: &str) -> LocationMatch {
        let syntax = match pattern {
            Pattern::Exact(value) => format!("= {value}"),
            Pattern::Prefix(value) => format!("^~ {value}"),
            Pattern::NormalPrefix(value) => value,
            Pattern::Regex(value) => format!("~ '{}'", value.as_str()),
            Pattern::CRegex(value) => format!("~* '{}'", value.as_str()),
            Pattern::Common => "/".to_owned(),
        };
        LocationMatch {
            location: parse_location_pattern(&syntax, "").unwrap(),
            access_log: crate::reverse::access_log::ActiveAccessLog::Disabled,
            matched: matched.to_owned(),
            remaining: remaining.to_owned(),
        }
    }

    fn proxy_pass(raw: &str, explicit: Option<&str>) -> ProxyPass {
        ProxyPass {
            raw: raw.to_owned(),
            uri: raw.parse().expect("proxy_pass uri"),
            proxy_host: raw
                .split("://")
                .nth(1)
                .unwrap()
                .split('/')
                .next()
                .unwrap()
                .to_owned(),
            explicit_path_and_query: explicit.map(str::to_owned),
        }
    }

    #[test]
    fn normalize_request_uri_collapses_slashes_and_dots() {
        let normalized = normalize_request_uri(&"/a//b/./c/../d?q=/x//y".parse().unwrap())
            .expect("normalized uri");
        assert_eq!(normalized.path, "/a/b/d");
        assert_eq!(normalized.query.as_deref(), Some("q=/x//y"));
    }

    #[test]
    fn build_upstream_target_preserves_original_path_when_proxy_pass_has_no_uri() {
        let normalized = NormalizedRequestUri {
            path: "/api/v1".to_string(),
            query: Some("q=1".to_string()),
        };
        let loc = make_location_match(Pattern::NormalPrefix("/api/".to_string()), "/api/", "v1");
        let target =
            build_upstream_request_target(&proxy_pass("http://backend", None), &loc, &normalized)
                .expect("target");
        assert_eq!(target, "/api/v1?q=1");
    }

    #[test]
    fn build_upstream_target_rewrites_prefix_when_proxy_pass_has_explicit_root_uri() {
        let normalized = NormalizedRequestUri {
            path: "/api/v1".to_string(),
            query: Some("q=1".to_string()),
        };
        let loc = make_location_match(Pattern::NormalPrefix("/api/".to_string()), "/api/", "v1");
        let target = build_upstream_request_target(
            &proxy_pass("http://backend/", Some("/")),
            &loc,
            &normalized,
        )
        .expect("target");
        assert_eq!(target, "/v1?q=1");
    }

    #[test]
    fn build_upstream_target_rewrites_exact_location_to_explicit_uri() {
        let normalized = NormalizedRequestUri {
            path: "/login".to_string(),
            query: None,
        };
        let loc = make_location_match(Pattern::Exact("/login".to_string()), "/login", "");
        let target = build_upstream_request_target(
            &proxy_pass("http://backend/auth/", Some("/auth/")),
            &loc,
            &normalized,
        )
        .expect("target");
        assert_eq!(target, "/auth/");
    }

    #[test]
    fn build_prefix_slash_redirect_appends_trailing_slash_and_preserves_query() {
        let normalized = NormalizedRequestUri {
            path: "/api".to_string(),
            query: Some("x=1".to_string()),
        };
        let target =
            build_prefix_slash_redirect("/api/", &normalized, Some("https://frontend.example.com"))
                .expect("redirect");
        assert_eq!(target, "https://frontend.example.com/api/?x=1");
    }
}

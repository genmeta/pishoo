use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use snafu::ResultExt;

use crate::parse::{
    ast::{AstBody, AstDirective},
    error::{ResolveIncludeError, resolve_include_error},
    grammar,
    source::{IncludeTrace, SourceMap},
};

pub fn expand_includes(
    directives: Vec<AstDirective>,
    source_map: &mut SourceMap,
    current_dir: Option<&Path>,
) -> Result<Vec<AstDirective>, ResolveIncludeError> {
    let mut expanded = Vec::new();
    for directive in directives {
        if directive.name.value == "include" {
            expand_include(directive, source_map, current_dir, &mut expanded)?;
        } else {
            expanded.push(expand_children(directive, source_map, current_dir)?);
        }
    }
    Ok(expanded)
}

fn expand_children(
    mut directive: AstDirective,
    source_map: &mut SourceMap,
    current_dir: Option<&Path>,
) -> Result<AstDirective, ResolveIncludeError> {
    if let AstBody::Block { children, .. } = &mut directive.body {
        let old_children = std::mem::take(children);
        *children = expand_includes(old_children, source_map, current_dir)?;
    }
    Ok(directive)
}

fn expand_include(
    directive: AstDirective,
    source_map: &mut SourceMap,
    current_dir: Option<&Path>,
    out: &mut Vec<AstDirective>,
) -> Result<(), ResolveIncludeError> {
    if !matches!(directive.body, AstBody::Leaf { .. }) {
        return resolve_include_error::InvalidShapeSnafu {
            span: directive.span,
        }
        .fail();
    }
    if directive.args.len() != 1 {
        return resolve_include_error::InvalidArgumentCountSnafu {
            span: directive.span,
            count: directive.args.len(),
        }
        .fail();
    }

    let pattern = &directive.args[0];
    let pattern_path = Path::new(&pattern.value);
    let resolved_pattern = if pattern_path.is_absolute() {
        pattern_path.to_path_buf()
    } else if let Some(current_dir) = current_dir {
        current_dir.join(pattern_path)
    } else {
        PathBuf::from(pattern_path)
    };

    let pattern_text = resolved_pattern.display().to_string();
    let entries = glob::glob(&pattern_text).context(resolve_include_error::GlobPatternSnafu {
        span: pattern.span,
        pattern: pattern_text.clone(),
    })?;

    for entry in entries {
        let entry = entry.context(resolve_include_error::GlobEntrySnafu {
            span: pattern.span,
            pattern: pattern_text.clone(),
        })?;
        let text =
            std::fs::read_to_string(&entry).context(resolve_include_error::ReadSourceSnafu {
                span: pattern.span,
                path: entry.clone(),
            })?;
        let text: Arc<str> = Arc::from(text);
        let child_dir = entry.parent();
        let source_id = source_map.add_source(
            Some(entry.clone()),
            Arc::clone(&text),
            child_dir.map(Path::to_path_buf),
            Some(IncludeTrace {
                parent: directive.span.source_id,
                directive_span: directive.span,
            }),
        );
        let parsed = grammar::parse_source(&text, source_id)
            .context(resolve_include_error::ParseSourceSnafu { source_id })?;
        out.extend(expand_includes(parsed, source_map, child_dir)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::parse::{grammar, source::SourceId};

    fn temp_config_dir(test_name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gateway-config-include-{test_name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temporary config directory should be created");
        dir
    }

    #[test]
    fn expand_includes_expands_file_and_records_trace() {
        let dir = temp_config_dir("expand");
        let child_path = dir.join("child.conf");
        fs::write(&child_path, "pid /tmp/pishoo.pid;").expect("child config should be written");

        let root_text = "include child.conf;";
        let mut source_map = SourceMap::default();
        let root = source_map.add_source(None, Arc::from(root_text), None, None);
        let directives = grammar::parse_source(root_text, root).expect("root should parse");

        let expanded = expand_includes(directives, &mut source_map, Some(&dir))
            .expect("include should expand");

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].name.value, "pid");
        let child = source_map
            .get(SourceId(1))
            .expect("included source should be registered");
        assert_eq!(child.path.as_deref(), Some(child_path.as_path()));
        let trace = child
            .included_from
            .as_ref()
            .expect("included source should record include trace");
        assert_eq!(trace.parent, root);

        fs::remove_dir_all(dir).expect("temporary config directory should be removed");
    }

    #[test]
    fn expand_includes_preserves_nested_include_trace() {
        let dir = temp_config_dir("nested");
        fs::write(dir.join("child.conf"), "include grandchild.conf;")
            .expect("child config should be written");
        fs::write(dir.join("grandchild.conf"), "pid /tmp/pishoo.pid;")
            .expect("grandchild config should be written");

        let root_text = "include child.conf;";
        let mut source_map = SourceMap::default();
        let root = source_map.add_source(None, Arc::from(root_text), None, None);
        let directives = grammar::parse_source(root_text, root).expect("root should parse");

        let expanded = expand_includes(directives, &mut source_map, Some(&dir))
            .expect("include should expand");

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].name.value, "pid");
        let child = source_map
            .get(SourceId(1))
            .expect("child source should be registered");
        let child_trace = child
            .included_from
            .as_ref()
            .expect("child source should record include trace");
        assert_eq!(child_trace.parent, root);
        let grandchild = source_map
            .get(SourceId(2))
            .expect("grandchild source should be registered");
        let grandchild_trace = grandchild
            .included_from
            .as_ref()
            .expect("grandchild source should record include trace");
        assert_eq!(grandchild_trace.parent, SourceId(1));

        fs::remove_dir_all(dir).expect("temporary config directory should be removed");
    }

    #[test]
    fn expand_includes_preserves_parse_source_error_trace() {
        let dir = temp_config_dir("parse-error");
        fs::write(dir.join("child.conf"), "server {\n").expect("child config should be written");

        let root_text = "include child.conf;";
        let mut source_map = SourceMap::default();
        let root = source_map.add_source(None, Arc::from(root_text), None, None);
        let directives = grammar::parse_source(root_text, root).expect("root should parse");

        let error = expand_includes(directives, &mut source_map, Some(&dir))
            .expect_err("included syntax error should fail");

        let ResolveIncludeError::ParseSource { source_id, .. } = error else {
            panic!("expected parse source error");
        };
        let child = source_map
            .get(source_id)
            .expect("failed included source should be registered");
        assert!(child.included_from.is_some());

        fs::remove_dir_all(dir).expect("temporary config directory should be removed");
    }

    #[test]
    fn expand_includes_reports_read_source_error() {
        let dir = temp_config_dir("read-error");
        let child_dir = dir.join("child.conf");
        fs::create_dir(&child_dir).expect("child directory should be created");

        let root_text = "include child.conf;";
        let mut source_map = SourceMap::default();
        let root = source_map.add_source(None, Arc::from(root_text), None, None);
        let directives = grammar::parse_source(root_text, root).expect("root should parse");

        let error = expand_includes(directives, &mut source_map, Some(&dir))
            .expect_err("directory include should fail to read as file");

        assert!(matches!(error, ResolveIncludeError::ReadSource { .. }));

        fs::remove_dir_all(dir).expect("temporary config directory should be removed");
    }
}

use std::ffi::OsString;

use snafu::Whatever;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSection {
    pub name: String,
    pub args: Vec<OsString>,
}

pub fn parse_grouped_targets(
    tokens: &[OsString],
    known_targets: &[&str],
) -> Result<Vec<TargetSection>, Whatever> {
    snafu::ensure_whatever!(!tokens.is_empty(), "at least one target is required");

    let mut sections = Vec::new();
    let mut current: Option<TargetSection> = None;

    for token in tokens {
        let Some(value) = token.to_str() else {
            match &mut current {
                Some(section) => {
                    section.args.push(token.clone());
                    continue;
                }
                None => snafu::whatever!("target name must be utf-8"),
            }
        };

        if known_targets.contains(&value) {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            current = Some(TargetSection {
                name: value.to_string(),
                args: Vec::new(),
            });
            continue;
        }

        match &mut current {
            Some(section) => section.args.push(token.clone()),
            None if value.starts_with('-') => {
                snafu::whatever!("expected a target before argument {value}")
            }
            None => snafu::whatever!("unknown target {value}"),
        }
    }

    if let Some(section) = current {
        sections.push(section);
    }

    snafu::ensure_whatever!(!sections.is_empty(), "at least one target is required");
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::parse_grouped_targets;

    fn os(value: &str) -> OsString {
        OsString::from(value)
    }

    #[test]
    fn splits_tokens_at_known_target_names() {
        let tokens = [
            os("apt"),
            os("--prefix"),
            os("ppa/genmeta"),
            os("rpm"),
            os("--prefix"),
            os("rpm/genmeta"),
            os("homebrew"),
        ];

        let sections = parse_grouped_targets(&tokens, &["apt", "rpm", "homebrew"])
            .expect("sections should parse");

        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].name, "apt");
        assert_eq!(sections[0].args, [os("--prefix"), os("ppa/genmeta")]);
        assert_eq!(sections[1].name, "rpm");
        assert_eq!(sections[1].args, [os("--prefix"), os("rpm/genmeta")]);
        assert_eq!(sections[2].name, "homebrew");
        assert!(sections[2].args.is_empty());
    }

    #[test]
    fn preserves_repeated_options_inside_a_section() {
        let tokens = [
            os("deb"),
            os("--target"),
            os("x86_64-unknown-linux-gnu"),
            os("--target"),
            os("aarch64-unknown-linux-gnu"),
            os("--sibling"),
            os("../dhttp"),
            os("--sibling"),
            os("../h3x"),
        ];

        let sections = parse_grouped_targets(&tokens, &["deb", "rpm", "homebrew"])
            .expect("sections should parse");

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "deb");
        assert_eq!(
            sections[0].args,
            [
                os("--target"),
                os("x86_64-unknown-linux-gnu"),
                os("--target"),
                os("aarch64-unknown-linux-gnu"),
                os("--sibling"),
                os("../dhttp"),
                os("--sibling"),
                os("../h3x"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_arguments_inside_a_section() {
        use std::os::unix::ffi::OsStringExt;

        let raw_path = OsString::from_vec(vec![b'.', b'.', b'/', 0xff, b'h', b'3', b'x']);
        let tokens = [os("deb"), os("--sibling"), raw_path.clone()];

        let sections = parse_grouped_targets(&tokens, &["deb"]).expect("sections should parse");

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "deb");
        assert_eq!(sections[0].args, [os("--sibling"), raw_path]);
    }

    #[test]
    fn rejects_empty_target_list() {
        let error =
            parse_grouped_targets(&[], &["apt", "rpm"]).expect_err("empty target list should fail");

        assert_eq!(error.to_string(), "at least one target is required");
    }

    #[test]
    fn rejects_tokens_before_first_target() {
        let tokens = [os("--prefix"), os("ppa/genmeta"), os("apt")];

        let error = parse_grouped_targets(&tokens, &["apt", "rpm"])
            .expect_err("target options before a target should fail");

        assert_eq!(
            error.to_string(),
            "expected a target before argument --prefix"
        );
    }

    #[test]
    fn rejects_unknown_target() {
        let tokens = [os("unknown"), os("--flag")];

        let error = parse_grouped_targets(&tokens, &["apt", "rpm"])
            .expect_err("unknown target should fail");

        assert_eq!(error.to_string(), "unknown target unknown");
    }
}

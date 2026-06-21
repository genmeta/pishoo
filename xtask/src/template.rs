use std::collections::BTreeMap;

use snafu::{Snafu, ensure};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RenderTemplateError {
    #[snafu(display("template variable {name} is not defined"))]
    MissingVariable { name: String },
    #[snafu(display("template contains unresolved placeholders"))]
    UnresolvedPlaceholder,
}

pub fn render_template(
    template: &str,
    variables: &BTreeMap<String, String>,
) -> Result<String, RenderTemplateError> {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("}}") else {
            return Err(RenderTemplateError::UnresolvedPlaceholder);
        };
        let name = after_start[..end].trim().to_string();
        let value = variables
            .get(&name)
            .ok_or(RenderTemplateError::MissingVariable { name })?;
        output.push_str(value);
        rest = &after_start[end + 2..];
    }
    output.push_str(rest);
    ensure!(
        !output.contains("{{") && !output.contains("}}"),
        render_template_error::UnresolvedPlaceholderSnafu
    );
    Ok(output)
}

pub fn ruby_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{render_template, ruby_string};

    #[test]
    fn renders_named_variables() {
        let variables = BTreeMap::from([
            ("package.name".to_string(), "pishoo".to_string()),
            ("package.version".to_string(), "0.6.1".to_string()),
        ]);

        let rendered = render_template("{{ package.name }} {{package.version}}", &variables)
            .expect("template should render");

        assert_eq!(rendered, "pishoo 0.6.1");
    }

    #[test]
    fn rejects_missing_variables() {
        let variables = BTreeMap::new();

        let error = render_template("{{package.name}}", &variables)
            .expect_err("missing variable should fail");

        assert_eq!(
            error.to_string(),
            "template variable package.name is not defined"
        );
    }

    #[test]
    fn escapes_ruby_strings() {
        assert_eq!(ruby_string("a\\\"b"), "a\\\\\\\"b");
    }
}

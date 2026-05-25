use snafu::{ResultExt, Snafu};

use crate::parse::{
    ast::{AstBody, AstDirective, Spanned},
    source::{SourceId, SourceSpan},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ParseSyntaxError {
    #[snafu(display("failed to parse configuration syntax"))]
    Syntax {
        source_id: SourceId,
        source: peg::error::ParseError<peg::str::LineCol>,
    },
}

pub fn parse_source(
    input: &str,
    source_id: SourceId,
) -> Result<Vec<AstDirective>, ParseSyntaxError> {
    config_grammar::file(input, source_id).context(parse_syntax_error::SyntaxSnafu { source_id })
}

fn span(source_id: SourceId, start: usize, end: usize) -> SourceSpan {
    SourceSpan::new(source_id, start, end)
}

peg::parser! {
    grammar config_grammar(source_id: SourceId) for str {
        pub rule file() -> Vec<AstDirective>
            = spacing() directives:directive()* spacing() ![_] { directives }

        rule directive() -> AstDirective
            = start:position!()
              name:literal()
              args:(spacing1() arg:literal() { arg })*
              spacing()
              body:body()
              end:position!()
              spacing()
              { AstDirective { name, args, body, span: span(source_id, start, end) } }

        rule body() -> AstBody
            = semicolon()
            / block()

        rule semicolon() -> AstBody
            = start:position!() ";" end:position!()
              { AstBody::Leaf { semicolon: span(source_id, start, end) } }

        rule block() -> AstBody
            = open_start:position!() "{" open_end:position!()
              spacing()
              children:directive()*
              spacing()
              close_start:position!() "}" close_end:position!()
              { AstBody::Block {
                    open: span(source_id, open_start, open_end),
                    children,
                    close: span(source_id, close_start, close_end),
                }
              }

        rule literal() -> Spanned<String>
            = double_quoted()
            / single_quoted()
            / bare()

        rule bare() -> Spanned<String>
            = start:position!()
              raw:$((!special() [_])+)
              end:position!()
              { Spanned::new(raw.to_owned(), span(source_id, start, end)) }

        rule double_quoted() -> Spanned<String>
            = start:position!() "\"" raw:$(double_char()*) "\"" end:position!()
              { Spanned::new(decode_quoted(raw, '"'), span(source_id, start, end)) }

        rule single_quoted() -> Spanned<String>
            = start:position!() "'" raw:$(single_char()*) "'" end:position!()
              { Spanned::new(decode_quoted(raw, '\''), span(source_id, start, end)) }

        rule double_char()
            = "\\\"" / "\\\\" / "\\\n" / !"\"" [_]

        rule single_char()
            = "\\'" / "\\\\" / "\\\n" / !"'" [_]

        rule special()
            = [' ' | '\t' | '\r' | '\n' | ';' | '{' | '}' | '"' | '\'' | '#']

        rule spacing()
            = quiet!{ ([' ' | '\t' | '\r' | '\n'] / comment())* }

        rule spacing1()
            = quiet!{ ([' ' | '\t' | '\r' | '\n'] / comment())+ }

        rule comment()
            = "#" [^'\n']* ("\n" / ![_])
    }
}

fn decode_quoted(raw: &str, quote: char) -> String {
    let mut decoded = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek().copied() {
                Some('\\') => {
                    chars.next();
                    decoded.push('\\');
                    continue;
                }
                Some('\n') => {
                    chars.next();
                    decoded.push('\n');
                    continue;
                }
                Some(next) if next == quote => {
                    chars.next();
                    decoded.push(quote);
                    continue;
                }
                _ => {}
            }
        }

        decoded.push(ch);
    }

    decoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ast::AstBody;

    #[test]
    fn parses_leaf_directive() {
        let parsed =
            parse_source("pid /tmp/pishoo.pid;", SourceId(0)).expect("parse should succeed");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name.value, "pid");
        assert_eq!(parsed[0].args[0].value, "/tmp/pishoo.pid");
        assert!(matches!(parsed[0].body, AstBody::Leaf { .. }));
    }

    #[test]
    fn parses_nested_block() {
        let parsed = parse_source("pishoo { server { listen all 443; } }", SourceId(0))
            .expect("parse should succeed");
        let AstBody::Block { children, .. } = &parsed[0].body else {
            panic!("pishoo should be a block");
        };
        assert_eq!(children[0].name.value, "server");
    }

    #[test]
    fn parses_multiple_sibling_directives() {
        let parsed = parse_source(
            "pid /tmp/pishoo.pid;\nworker admin { server { listen all 443; } }",
            SourceId(0),
        )
        .expect("parse should succeed");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name.value, "pid");
        assert_eq!(parsed[1].name.value, "worker");
    }

    #[test]
    fn parses_comments_and_quotes() {
        let parsed = parse_source("# top\npid \"/tmp/pishoo pid\";\n", SourceId(0))
            .expect("parse should succeed");
        assert_eq!(parsed[0].args[0].value, "/tmp/pishoo pid");
    }

    #[test]
    fn decodes_quoted_escapes_left_to_right() {
        let parsed = parse_source(r#"pid "\\\"";"#, SourceId(0)).expect("parse should succeed");
        assert_eq!(parsed[0].args[0].value, "\\\"");

        let parsed = parse_source(r#"pid '\\\'';"#, SourceId(0)).expect("parse should succeed");
        assert_eq!(parsed[0].args[0].value, "\\'");

        let parsed = parse_source("pid \"\\\\\n\";", SourceId(0)).expect("parse should succeed");
        assert_eq!(parsed[0].args[0].value, "\\\n");
    }

    #[test]
    fn rejects_unclosed_block() {
        let error = parse_source("pishoo { server {", SourceId(0)).expect_err("parse should fail");
        assert!(!error.to_string().contains('\n'));
    }
}

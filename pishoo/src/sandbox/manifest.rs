use std::collections::HashSet;

use wasmparser::{Encoding, Parser, Payload};

use crate::{Error, Result, routes::reserved};

pub fn validate_lib(bytes: &[u8]) -> Result<oas3::OpenApiV3Spec> {
    let invalid = |s: &str| Error::InvalidComponent(s.into());
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(invalid("component exceeds 64 MiB"));
    }
    let mut document = None;
    let mut depth = 0;
    let mut component = false;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| Error::InvalidComponent(e.to_string()))? {
            Payload::Version {
                encoding: Encoding::Component,
                ..
            } if depth == 0 => component = true,
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) if depth > 0 => depth -= 1,
            Payload::CustomSection(section) if depth == 0 && section.name() == "pishoo:openapi" => {
                if document.is_some() {
                    return Err(invalid("duplicate pishoo:openapi section"));
                }
                if section.data().len() > 1024 * 1024 {
                    return Err(invalid("OpenAPI exceeds 1 MiB"));
                }
                document = Some(section.data());
            }
            _ => {}
        }
    }
    if !component {
        return Err(invalid("lib.wasm must be a component"));
    }
    let document = document.ok_or_else(|| invalid("missing pishoo:openapi section"))?;
    // Parsing first bounds recursion and validates JSON before the duplicate-key walk.
    let value: serde_json::Value =
        serde_json::from_slice(document).map_err(|e| Error::InvalidComponent(e.to_string()))?;
    check_json_keys(document, &mut 0)?;
    check_refs(&value)?;
    let openapi: oas3::OpenApiV3Spec =
        serde_json::from_value(value).map_err(|e| Error::InvalidComponent(e.to_string()))?;
    if !openapi.openapi.starts_with("3.1.") {
        return Err(invalid("only OpenAPI 3.1.x is supported"));
    }
    let paths = openapi
        .paths
        .as_ref()
        .ok_or_else(|| invalid("missing OpenAPI paths"))?;
    let mut ids = HashSet::new();
    for (path, item) in paths {
        if !valid_path(path)
            || path.contains(['{', '}'])
            || reserved(path, &["/api", "/.pishoo", "/exec"])
        {
            return Err(invalid("unsupported or reserved API path"));
        }
        if item.reference.is_some() {
            return Err(invalid("path-level references are unsupported"));
        }
        for (_, operation) in item.methods() {
            if let Some(id) = &operation.operation_id
                && !ids.insert(id)
            {
                return Err(invalid("duplicate operationId"));
            }
        }
    }
    Ok(openapi)
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.split('/').any(|p| p == "." || p == "..")
        && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn check_refs(value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Object(values) => {
            if values
                .get("$ref")
                .is_some_and(|v| v.as_str().is_none_or(|s| !s.starts_with("#/")))
            {
                return Err(Error::InvalidComponent(
                    "external references are unsupported".into(),
                ));
            }
            for value in values.values() {
                check_refs(value)?;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                check_refs(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}
// This walks already validated JSON. Key strings are decoded before comparison.
fn check_json_keys(bytes: &[u8], index: &mut usize) -> Result<()> {
    skip_space(bytes, index);
    match bytes[*index] {
        b'{' => {
            *index += 1;
            let mut keys = HashSet::new();
            loop {
                skip_space(bytes, index);
                if bytes[*index] == b'}' {
                    *index += 1;
                    break;
                }
                let start = *index;
                skip_string(bytes, index);
                let key: String =
                    serde_json::from_slice(&bytes[start..*index]).expect("validated JSON string");
                if !keys.insert(key) {
                    return Err(Error::InvalidComponent("duplicate JSON key".into()));
                }
                skip_space(bytes, index);
                *index += 1; // colon
                check_json_keys(bytes, index)?;
                skip_space(bytes, index);
                if bytes[*index] == b',' {
                    *index += 1;
                }
            }
        }
        b'[' => {
            *index += 1;
            loop {
                skip_space(bytes, index);
                if bytes[*index] == b']' {
                    *index += 1;
                    break;
                }
                check_json_keys(bytes, index)?;
                skip_space(bytes, index);
                if bytes[*index] == b',' {
                    *index += 1;
                }
            }
        }
        b'"' => skip_string(bytes, index),
        _ => {
            while *index < bytes.len()
                && !matches!(
                    bytes[*index],
                    b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t'
                )
            {
                *index += 1;
            }
        }
    }
    Ok(())
}
fn skip_space(bytes: &[u8], index: &mut usize) {
    while *index < bytes.len() && bytes[*index].is_ascii_whitespace() {
        *index += 1;
    }
}
fn skip_string(bytes: &[u8], index: &mut usize) {
    *index += 1;
    while bytes[*index] != b'"' {
        if bytes[*index] == b'\\' {
            *index += 1;
        }
        *index += 1;
    }
    *index += 1;
}

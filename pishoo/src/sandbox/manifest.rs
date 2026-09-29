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
    let openapi: oas3::OpenApiV3Spec =
        serde_json::from_slice(document).map_err(|e| Error::InvalidComponent(e.to_string()))?;
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

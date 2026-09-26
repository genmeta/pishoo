//! Load each deployed component from one immutable file snapshot.

use std::{collections::HashSet, fs};

use dhttp_home::identity::IdentityProfile;
use oas3::OpenApiV3Spec;
use wasmparser::{Encoding, Parser, Payload};

use super::{
    Result,
    discovery::{direct_directories, valid_lib_id},
    invalid,
};
use crate::wasm::WasmLib;

pub(crate) fn discover_libs(profile: &IdentityProfile) -> Result<Vec<WasmLib>> {
    let mut libs = Vec::new();
    let mut seen = HashSet::new();
    for (id, path) in direct_directories(&profile.join("lib"))? {
        if !valid_lib_id(&id) {
            eprintln!("skipping invalid lib directory: {}", path.display());
            continue;
        }
        if !seen.insert(id.to_ascii_lowercase()) {
            return Err(invalid("lib id collision").into());
        }
        let wasm = path.join("lib.wasm");
        if !wasm.is_file() || wasm.symlink_metadata()?.file_type().is_symlink() {
            continue;
        }
        if fs::metadata(&wasm)?.len() > MAX_WASM_BYTES {
            eprintln!("skipping oversized lib: {}", wasm.display());
            continue;
        }
        match fs::read(&wasm)
            .and_then(|bytes| WasmLib::new(id, &bytes).map_err(|error| invalid(error.to_string())))
        {
            Ok(lib) => libs.push(lib),
            Err(error) => eprintln!("skipping invalid lib {}: {error}", wasm.display()),
        }
    }
    Ok(libs)
}

const MAX_WASM_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OPENAPI_BYTES: usize = 1024 * 1024;

/// Validate one immutable component snapshot and return its structured OpenAPI document.
pub fn validate_lib(bytes: &[u8]) -> Result<OpenApiV3Spec> {
    if bytes.len() as u64 > MAX_WASM_BYTES {
        return Err(invalid("component exceeds 64 MiB").into());
    }
    let mut document = None;
    let mut depth = 0;
    let mut component = false;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::Version {
                encoding: Encoding::Component,
                ..
            } if depth == 0 => component = true,
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) if depth > 0 => depth -= 1,
            Payload::CustomSection(section) if depth == 0 => {
                if section.name() == "pishoo:openapi" {
                    if document.is_some() {
                        return Err(invalid("duplicate pishoo:openapi section").into());
                    }
                    if section.data().len() > MAX_OPENAPI_BYTES {
                        return Err(invalid("OpenAPI exceeds 1 MiB").into());
                    }
                    document = Some(section.data().to_vec());
                }
            }
            _ => {}
        }
    }
    if !component {
        return Err(invalid("lib.wasm must be a WebAssembly component").into());
    }
    let document = document.ok_or_else(|| invalid("missing pishoo:openapi section"))?;
    let openapi: OpenApiV3Spec = serde_json::from_slice(&document)?;
    if !openapi.openapi.starts_with("3.1.") {
        return Err(invalid("only OpenAPI 3.1.x is supported").into());
    }
    let mut ids = HashSet::new();
    let paths = openapi
        .paths
        .as_ref()
        .ok_or_else(|| invalid("missing OpenAPI paths"))?;
    for (path, item) in paths {
        if !valid_lib_path(path) {
            return Err(invalid(format!("unsupported API path {path}")).into());
        }
        if item.reference.is_some() {
            return Err(invalid("path-level $ref is unsupported").into());
        }
        for (_, operation) in item.methods() {
            if let Some(id) = &operation.operation_id {
                if !ids.insert(id) {
                    return Err(invalid("duplicate operationId").into());
                }
            }
        }
    }
    Ok(openapi)
}

fn valid_lib_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\', '{', '}'])
        && !path.split('/').any(|part| part == "." || part == "..")
        && !path.chars().any(char::is_whitespace)
}

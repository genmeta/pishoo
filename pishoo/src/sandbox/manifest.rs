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
    let mut section_end = 8;
    let mut signature = None;
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| Error::InvalidComponent(e.to_string()))?;
        let next_end = if depth == 0 {
            payload.as_section().map(|(_, range)| range.end as usize)
        } else {
            None
        };
        match payload {
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
            Payload::CustomSection(section) if section.name() == "pishoo:signature" => {
                if depth != 0
                    || signature.is_some()
                    || section.range().end as usize != bytes.len()
                    || section.data().len() > 64 * 1024
                {
                    return Err(invalid("signature must be a unique bounded final section"));
                }
                signature = Some((section_end, section.data()));
            }
            _ => {}
        }
        if let Some(end) = next_end {
            section_end = end;
        }
    }
    if !component {
        return Err(invalid("lib.wasm must be a component"));
    }
    let document = document.ok_or_else(|| invalid("missing pishoo:openapi section"))?;
    if let Some((signed_end, envelope)) = signature {
        verify_component_signature(bytes, signed_end, envelope, document)?;
    }
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
        if !valid_path(path) || path.contains(['{', '}']) || reserved(path) {
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

// Historical certificates authenticate signed bytes here. Online store
// installation additionally authenticates the publishing proof through the
// pinned store; startup does not apply today's expiry to an old release.
fn verify_component_signature(
    bytes: &[u8],
    signed_end: usize,
    envelope: &[u8],
    document: &[u8],
) -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    let invalid = |message: &str| Error::InvalidComponent(message.into());
    let envelope: serde_json::Value =
        serde_json::from_slice(envelope).map_err(|_| invalid("invalid signature envelope"))?;
    if envelope.as_object().is_none_or(|object| object.len() != 3)
        || envelope["version"].as_u64() != Some(1)
    {
        return Err(invalid("unsupported signature envelope"));
    }
    let certificates = envelope["certificates"]
        .as_array()
        .filter(|values| !values.is_empty())
        .ok_or_else(|| invalid("missing signer certificates"))?;
    let certificates = certificates
        .iter()
        .map(|value| {
            STANDARD
                .decode(
                    value
                        .as_str()
                        .ok_or_else(|| invalid("invalid signer certificate"))?,
                )
                .map(qtls::CertificateDer::from)
                .map_err(|_| invalid("invalid signer certificate encoding"))
        })
        .collect::<Result<Vec<_>>>()?;
    let (remaining, certificate) = x509_parser::parse_x509_certificate(certificates[0].as_ref())
        .map_err(|_| invalid("invalid signer certificate"))?;
    if !remaining.is_empty() {
        return Err(invalid("trailing signer certificate data"));
    }
    let document: serde_json::Value =
        serde_json::from_slice(document).map_err(|_| invalid("invalid signed OpenAPI"))?;
    let publisher = document["x-pishoo"]["publisher"]
        .as_str()
        .filter(|name| dhttp_home::normalize_name(name).as_deref() == Some(*name))
        .ok_or_else(|| invalid("invalid signed publisher"))?;
    dhttp_home::certificate::extract_dhttp_subject_key_identifier(&certificates)
        .map_err(|_| invalid("invalid signer owner_hash"))?;
    if certificate.subject_alternative_name().map_err(|_| invalid("invalid signer names"))?
        .is_none_or(|names| !names.value.general_names.iter().any(|name| {
            matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if *name == publisher)
        }))
    {
        return Err(invalid("signer certificate and publisher disagree"));
    }
    let signature = STANDARD
        .decode(
            envelope["signature"]
                .as_str()
                .ok_or_else(|| invalid("missing signature"))?,
        )
        .map_err(|_| invalid("invalid signature encoding"))?;
    if signature.is_empty() || signature.len() > 8192 {
        return Err(invalid("invalid signature size"));
    }
    let mut input = b"pishoo-lib-signature-v1\0".to_vec();
    input.extend_from_slice(&Sha256::digest(&bytes[..signed_end]));
    if !dhttp_home::certificate::verify_signature(certificate.public_key().raw, &input, &signature)
        .map_err(|_| invalid("unsupported publisher signing key"))?
    {
        return Err(invalid("publisher signature verification failed"));
    }
    Ok(())
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.split('/').any(|p| p == "." || p == "..")
        && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}

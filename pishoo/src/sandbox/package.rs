//! Pishoo WEBC v3 profile: immutable container, publication signature and route binding.
// Protocol matches the independent store's WEBC package validation.
use crate::{Error, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
const PACKAGE_LIMIT: usize = 64 * 1024 * 1024;
const METHODS: [&str; 8] = [
    "get", "head", "post", "put", "patch", "delete", "options", "trace",
];
fn invalid(message: &str) -> Error {
    Error::InvalidComponent(message.into())
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid(&format!("missing or empty {key}")))
}
fn webc_u64(bytes: &[u8], offset: usize) -> Result<usize> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| invalid("WEBC length overflow"))?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("truncated WEBC length"))?
        .try_into()
        .unwrap();
    usize::try_from(u64::from_le_bytes(raw)).map_err(|_| invalid("WEBC length overflow"))
}

pub(super) fn read_package(bytes: &[u8]) -> Result<webc::Container> {
    // The dependency has lazy readers. Bound and authenticate every top-level span
    // before using them, including rejecting unindexed sections and trailing bytes.
    if bytes.len() > PACKAGE_LIMIT
        || bytes.get(..8) != Some(b"\0webc003")
        || bytes.get(8) != Some(&2)
    {
        return Err(invalid("expected a bounded WEBC v3 package"));
    }
    let end = 17usize
        .checked_add(webc_u64(bytes, 9)?)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid("truncated WEBC index"))?;
    if end > 1024 * 1024 {
        return Err(invalid("WEBC index exceeds 1 MiB"));
    }
    let section = webc::v3::read::Section::parse(2, None, bytes[17..end].to_vec().into())
        .map_err(|_| invalid("invalid WEBC index"))?;
    let index = section
        .as_index()
        .unwrap()
        .index()
        .map_err(|_| invalid("invalid WEBC index"))?;
    if !index.signature.is_none() || !index.signature.value.is_empty() || index.volumes.len() > 16 {
        return Err(invalid("unsupported WEBC index signature or volume count"));
    }
    let mut spans = BTreeMap::new();
    for (tag, name, entry) in std::iter::once((1, "", &index.manifest))
        .chain(std::iter::once((3, "", &index.atoms)))
        .chain(
            index
                .volumes
                .iter()
                .map(|(name, entry)| (4, name.as_str(), entry)),
        )
    {
        let stop = entry
            .span
            .start
            .checked_add(entry.span.len)
            .filter(|stop| *stop <= bytes.len())
            .ok_or_else(|| invalid("WEBC span exceeds input"))?;
        if entry.span.start < end
            || entry.span.len < 41
            || spans.insert(entry.span.start, stop).is_some()
        {
            return Err(invalid("overlapping WEBC sections"));
        }
        let raw = &bytes[entry.span.start..stop];
        if raw[0] != tag || webc_u64(raw, 33)? != raw.len() - 41 {
            return Err(invalid("WEBC section disagrees with index"));
        }
        let hash = Sha256::digest(&raw[41..]);
        if raw[1..33] != hash[..] || entry.checksum != webc::v3::Checksum::sha256(hash.into()) {
            return Err(invalid("WEBC section checksum mismatch"));
        }
        if tag == 1 && raw.len() > 1024 * 1024 {
            return Err(invalid("WEBC manifest exceeds 1 MiB"));
        }
        if tag == 4 {
            let name_end = 8usize
                .checked_add(webc_u64(&raw[41..], 0)?)
                .filter(|n| *n <= raw.len() - 41)
                .ok_or_else(|| invalid("invalid WEBC volume name"))?;
            if &raw[49..41 + name_end] != name.as_bytes() {
                return Err(invalid("WEBC volume name disagrees with index"));
            }
        }
    }
    let mut position = end;
    for (start, stop) in spans {
        if start != position {
            return Err(invalid("unindexed or overlapping WEBC bytes"));
        }
        position = stop;
    }
    if position != bytes.len() {
        return Err(invalid("unindexed trailing WEBC bytes"));
    }
    let container =
        webc::Container::from_bytes_and_version(bytes.to_vec().into(), webc::Version::V3)
            .map_err(|_| invalid("malformed WEBC package"))?;
    // Use an iterative bounded walk before any recursive dependency validator.
    let mut count = 0;
    for volume in container.volumes().values() {
        volume_files(volume, "/", &mut count)?;
    }
    container
        .validate()
        .map_err(|_| invalid("invalid WEBC atom, mapping or file digest"))?;
    let manifest = container.manifest();
    if !manifest.use_map.is_empty()
        || !manifest.commands.is_empty()
        || manifest.entrypoint.is_some()
        || !manifest.bindings.is_empty()
        || manifest.atoms.len() != 1
        || !manifest.atoms.contains_key("backend")
        || container.atoms().len() != 1
        || container.get_atom("backend").is_none()
    {
        return Err(invalid(
            "package must contain one backend atom without commands, bindings or dependencies",
        ));
    }
    let mappings = manifest
        .filesystem()
        .map_err(|_| invalid("invalid WEBC filesystem mappings"))?
        .ok_or_else(|| invalid("missing WEBC filesystem mappings"))?;
    if mappings.len() != 2
        || !mappings.iter().any(|m| m.mount_path == "/public")
        || !mappings.iter().any(|m| m.mount_path == "/metadata")
        || mappings
            .iter()
            .any(|m| m.from.is_some() || m.volume_name == "pishoo-signature")
    {
        return Err(invalid(
            "require exactly local /public and /metadata directory mappings",
        ));
    }
    for mapping in mappings.iter() {
        let root = mapping.host_path.as_deref().unwrap_or("/");
        if !file_path(root)
            || container
                .get_volume(&mapping.volume_name)
                .and_then(|v| v.read_dir(root))
                .is_none()
        {
            return Err(invalid("invalid mapped WEBC directory"));
        }
    }
    let backend = container.get_atom("backend").unwrap();
    if backend.get(..8) != Some(b"\0asm\x0d\0\x01\0") {
        return Err(invalid("backend must be a WASM component"));
    }
    for item in wasmparser::Parser::new(0).parse_all(&backend) {
        if let wasmparser::Payload::CustomSection(section) =
            item.map_err(|_| invalid("malformed backend"))?
            && matches!(section.name(), "pishoo:openapi" | "pishoo:signature")
        {
            return Err(invalid(
                "backend must not embed a second OpenAPI or publisher signature",
            ));
        }
    }
    let raw = package_file(&container, "/metadata/openapi.json")?;
    super::validate_openapi(&raw)?;
    let document: Value =
        serde_json::from_slice(&raw).map_err(|_| invalid("invalid OpenAPI JSON"))?;
    validate_content(&container, &document)?;
    validate_declarations(&container.get_atom("backend").unwrap(), &document)?;
    verify_signature(bytes, &container, &document)?;
    Ok(container)
}

fn file_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.chars().any(|c| c.is_control() || c.is_whitespace())
        && !path.split('/').any(|s| s == "." || s == "..")
        && path.len() <= 1024
        && (path == "/" || !path.ends_with('/'))
}

fn volume_files(
    volume: &webc::Volume,
    root: &str,
    count: &mut usize,
) -> Result<BTreeMap<String, usize>> {
    let mut files = BTreeMap::new();
    let mut directories = vec![(root.to_owned(), 0)];
    while let Some((path, depth)) = directories.pop() {
        if depth > 32 {
            return Err(invalid("WEBC directory depth exceeds 32"));
        }
        let entries = volume
            .read_dir(path.as_str())
            .ok_or_else(|| invalid("unreadable WEBC directory"))?;
        let mut names = HashSet::new();
        for (name, _, metadata) in entries {
            *count += 1;
            if *count > 4096 || !names.insert(name.to_string()) {
                return Err(invalid("too many or duplicate WEBC entries"));
            }
            let child = format!("{}/{}", path.trim_end_matches('/'), name);
            if !file_path(&child) {
                return Err(invalid("invalid WEBC file path"));
            }
            match metadata {
                webc::Metadata::Dir { .. } => directories.push((child, depth + 1)),
                webc::Metadata::File { length, .. } => {
                    if length > 16 * 1024 * 1024 {
                        return Err(invalid("WEBC file exceeds 16 MiB"));
                    }
                    let (data, hash) = volume
                        .read_file(child.as_str())
                        .ok_or_else(|| invalid("unreadable WEBC file"))?;
                    if data.len() != length || hash != Some(Sha256::digest(&data).into()) {
                        return Err(invalid("WEBC file checksum mismatch"));
                    }
                    files.insert(child, length);
                }
                _ => return Err(invalid("WEBC symlinks are unsupported")),
            }
        }
    }
    Ok(files)
}

pub(super) fn package_file(container: &webc::Container, path: &str) -> Result<bytes::Bytes> {
    if !file_path(path) {
        return Err(invalid("invalid logical package path"));
    }
    let mappings = container
        .manifest()
        .filesystem()
        .map_err(|_| invalid("invalid mappings"))?
        .ok_or_else(|| invalid("missing mappings"))?;
    let mapping = mappings
        .iter()
        .find(|m| {
            path.strip_prefix(&m.mount_path)
                .is_some_and(|tail| tail.starts_with('/'))
        })
        .ok_or_else(|| invalid("path has no package mapping"))?;
    let tail = path.strip_prefix(&mapping.mount_path).unwrap();
    let target = format!(
        "{}{}",
        mapping
            .host_path
            .as_deref()
            .unwrap_or("/")
            .trim_end_matches('/'),
        tail
    );
    let volume = container
        .get_volume(&mapping.volume_name)
        .ok_or_else(|| invalid("missing mapped volume"))?;
    volume
        .read_file(target.as_str())
        .map(|(data, _)| bytes::Bytes::copy_from_slice(&data))
        .ok_or_else(|| invalid("missing mapped file"))
}

// Hash raw authenticated content sections, preserving every CBOR field, file,
// path and timestamp. The index and unmapped signature volume are excluded;
// they must already have passed the strict span/checksum checks above.
fn package_payload(bytes: &[u8]) -> String {
    let reader = webc::v3::read::OwnedReader::parse(bytes.to_vec()).unwrap();
    let index = reader.index();
    let mut hash = Sha256::new();
    hash.update(b"\0webc003");
    for entry in std::iter::once(&index.manifest)
        .chain(std::iter::once(&index.atoms))
        .chain(
            index
                .volumes
                .iter()
                .filter(|(name, _)| name.as_str() != "pishoo-signature")
                .map(|(_, entry)| entry),
        )
    {
        hash.update(&bytes[entry.span.start..entry.span.end()]);
    }
    format!("{:x}", hash.finalize())
}

fn verify_signature(bytes: &[u8], container: &webc::Container, document: &Value) -> Result<()> {
    let volume = container
        .get_volume("pishoo-signature")
        .ok_or_else(|| invalid("missing publisher signature volume"))?;
    let mut count = 0;
    let files = volume_files(&volume, "/", &mut count)?;
    if files.len() != 1
        || count != 1
        || files
            .get("/signature.json")
            .is_none_or(|size| *size > 64 * 1024)
    {
        return Err(invalid(
            "signature volume must contain only a bounded signature.json",
        ));
    }
    let (data, _) = volume.read_file("/signature.json").unwrap();
    let envelope: Value =
        serde_json::from_slice(&data).map_err(|_| invalid("invalid signature envelope"))?;
    if envelope.as_object().is_none_or(|o| o.len() != 3)
        || envelope["version"].as_u64() != Some(1)
        || envelope["certificates"]
            .as_array()
            .is_none_or(Vec::is_empty)
    {
        return Err(invalid("unsupported signature envelope"));
    }
    let signature = STANDARD
        .decode(text(&envelope, "signature")?)
        .map_err(|_| invalid("invalid signature encoding"))?;
    if signature.is_empty() || signature.len() > 8192 {
        return Err(invalid("invalid signature size"));
    }
    for cert in envelope["certificates"].as_array().unwrap() {
        if STANDARD
            .decode(
                cert.as_str()
                    .ok_or_else(|| invalid("invalid certificate encoding"))?,
            )
            .map_err(|_| invalid("invalid certificate encoding"))?
            .is_empty()
        {
            return Err(invalid("empty signer certificate"));
        }
    }
    let certificates = envelope["certificates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| {
            STANDARD
                .decode(value.as_str().unwrap())
                .map(qtls::CertificateDer::from)
                .map_err(|_| invalid("invalid signer certificate"))
        })
        .collect::<Result<Vec<_>>>()?;
    let (remaining, certificate) = x509_parser::parse_x509_certificate(certificates[0].as_ref())
        .map_err(|_| invalid("invalid signer certificate"))?;
    if !remaining.is_empty() {
        return Err(invalid("trailing signer certificate data"));
    }
    let publisher = document["x-pishoo"]["publisher"].as_str().unwrap();
    dhttp_home::certificate::extract_dhttp_subject_key_identifier(&certificates)
        .map_err(|_| invalid("invalid signer owner_hash"))?;
    if certificate.subject_alternative_name().map_err(|_| invalid("invalid signer names"))?
        .is_none_or(|names| !names.value.general_names.iter().any(|name|
            matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if *name == publisher))) {
        return Err(invalid("signer certificate and publisher disagree"));
    }
    let payload = package_payload(bytes);
    let mut input = b"pishoo-webc-signature-v1\0".to_vec();
    for i in (0..payload.len()).step_by(2) {
        input.push(u8::from_str_radix(&payload[i..i + 2], 16).unwrap());
    }
    if !dhttp_home::certificate::verify_signature(certificate.public_key().raw, &input, &signature)
        .map_err(|_| invalid("unsupported publisher signing key"))?
    {
        return Err(invalid("publisher signature verification failed"));
    }
    Ok(())
}

fn validate_content(container: &webc::Container, document: &Value) -> Result<()> {
    let info = container
        .manifest()
        .wapm()
        .map_err(|_| invalid("invalid package metadata"))?
        .ok_or_else(|| invalid("missing package metadata"))?;
    let extension = &document["x-pishoo"];
    let publisher = text(extension, "publisher")?;
    let name = text(extension, "name")?;
    super::valid_id(&format!("{publisher}/{name}"))
        .map_err(|_| invalid("invalid package identity"))?;
    let version = text(&document["info"], "version")?;
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.parse::<u64>().is_err()
                || !p.bytes().all(|b| b.is_ascii_digit())
                || p.len() > 1 && p.starts_with('0')
        })
    {
        return Err(invalid("version must be MAJOR.MINOR.PATCH"));
    }
    if extension.get("templates").is_some() {
        return Err(invalid("external templates are unsupported"));
    }
    let id = format!(
        "{}/{}",
        text(extension, "publisher")?,
        text(extension, "name")?
    );
    if info.name.as_deref() != Some(&id)
        || info.version.as_deref() != document["info"]["version"].as_str()
    {
        return Err(invalid("WEBC and OpenAPI identity/version disagree"));
    }
    let mappings = container
        .manifest()
        .filesystem()
        .map_err(|_| invalid("invalid mappings"))?
        .unwrap();
    let mapping = mappings.iter().find(|m| m.mount_path == "/public").unwrap();
    let volume = container.get_volume(&mapping.volume_name).unwrap();
    let root = mapping.host_path.as_deref().unwrap_or("/");
    let mut count = 0;
    let files = volume_files(&volume, root, &mut count)?;
    let mut routes = BTreeMap::new();
    for file in files.keys() {
        let relative = file.strip_prefix(root.trim_end_matches('/')).unwrap();
        let logical = format!("/public{relative}");
        routes.insert(
            if relative == "/index.html" {
                "/".to_owned()
            } else {
                relative.to_owned()
            },
            logical,
        );
    }
    for (path, item) in document["paths"]
        .as_object()
        .ok_or_else(|| invalid("missing paths"))?
    {
        for method in METHODS {
            if let Some(operation) = item.get(method) {
                let file = operation["x-pishoo"].get("static-file");
                if let Some(expected) = routes.get(path) {
                    if !matches!(method, "get" | "head")
                        || file.and_then(Value::as_str) != Some(expected.as_str())
                    {
                        return Err(invalid("static file and backend route conflict"));
                    }
                } else if file.is_some() {
                    return Err(invalid("static operation has no matching public file"));
                }
            }
        }
    }
    for (path, file) in routes {
        for method in ["get", "head"] {
            if document["paths"][&path][method]["x-pishoo"]["static-file"] != file {
                return Err(invalid(
                    "every public file requires matching GET and HEAD operations",
                ));
            }
        }
    }
    if files
        .keys()
        .any(|f| f == &format!("{}/index.html", root.trim_end_matches('/')))
        && extension["entry"] != "/"
    {
        return Err(invalid("public/index.html requires entry /"));
    }
    Ok(())
}

fn valid_name(value: &str) -> bool {
    value.len() <= 63
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn validate_declarations(bytes: &[u8], document: &Value) -> Result<()> {
    let info = &document["info"];
    text(info, "title")?;
    text(info, "description")?;
    text(&info["license"], "name")?;
    let extension = &document["x-pishoo"];
    if extension.get("templates").is_some() {
        return Err(invalid("external templates are not part of store v1"));
    }
    let host = extension["host"]
        .as_object()
        .ok_or_else(|| invalid("host capability descriptions are required"))?;
    for (key, value) in host {
        if !matches!(
            key.as_str(),
            "private-storage" | "request-identity" | "signatures"
        ) || value.as_str().is_none_or(|s| s.trim().is_empty())
        {
            return Err(invalid("unknown or unexplained host capability"));
        }
    }
    let mut depth = 0;
    for item in wasmparser::Parser::new(0).parse_all(bytes) {
        match item.map_err(|_| invalid("malformed component imports"))? {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            wasmparser::Payload::ComponentImportSection(imports) if depth == 0 => {
                for import in imports {
                    let import = import.map_err(|_| invalid("malformed import"))?;
                    let capability = if import.name.name.starts_with("wasi:filesystem/") {
                        Some("private-storage")
                    } else if import
                        .name
                        .name
                        .starts_with("pishoo:identity/request-identity@")
                    {
                        Some("request-identity")
                    } else if import.name.name.starts_with("pishoo:identity/signatures@") {
                        Some("signatures")
                    } else {
                        None
                    };
                    if capability.is_some_and(|key| !host.contains_key(key)) {
                        return Err(invalid(
                            "imported host capability has no purpose declaration",
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let paths = document["paths"]
        .as_object()
        .ok_or_else(|| invalid("paths must be an object"))?;
    let mut operations = BTreeMap::new();
    let mut annotated = BTreeMap::<String, HashSet<String>>::new();
    for (path, item) in paths {
        for method in METHODS {
            let Some(operation) = item.get(method) else {
                continue;
            };
            let id = text(operation, "operationId")?;
            text(operation, "summary")?;
            if operations.insert(id.to_owned(), (path, method)).is_some() {
                return Err(invalid("duplicate operationId"));
            }
            let op_extension = &operation["x-pishoo"];
            if op_extension.get("template").is_some() {
                return Err(invalid("template operations are unsupported"));
            }
            if let Some(group) = op_extension.get("access-group") {
                let group = group
                    .as_str()
                    .filter(|s| valid_name(s))
                    .ok_or_else(|| invalid("invalid access-group"))?;
                annotated
                    .entry(group.to_owned())
                    .or_default()
                    .insert(id.to_owned());
            }
            if let Some(initial) = op_extension.get("initial-access") {
                let initial = initial
                    .as_array()
                    .ok_or_else(|| invalid("initial-access must be an array"))?;
                let mut grantees = HashSet::new();
                for rule in initial {
                    if rule.as_object().is_none_or(|o| o.len() != 2) {
                        return Err(invalid("initial-access requires only grantee and effect"));
                    }
                    let grantee = text(rule, "grantee")?;
                    grantee
                        .parse::<access_control::Grantee>()
                        .map_err(|_| invalid("invalid grantee"))?;
                    text(rule, "effect")?
                        .parse::<access_control::Effect>()
                        .map_err(|_| invalid("invalid effect"))?;
                    if !grantees.insert(grantee) {
                        return Err(invalid("duplicate initial-access grantee"));
                    }
                }
                if grantees.contains("*?") && (grantees.contains("**") || grantees.contains("?")) {
                    return Err(invalid("conflicting wildcard grantees"));
                }
            }
        }
    }
    let mut declared = BTreeMap::<String, HashSet<String>>::new();
    let mut members = HashSet::new();
    if let Some(groups) = extension.get("access-groups") {
        for (name, group) in groups
            .as_object()
            .ok_or_else(|| invalid("access-groups must be an object"))?
        {
            if !valid_name(name) {
                return Err(invalid("invalid group name"));
            }
            text(group, "title")?;
            let ids = group["operations"]
                .as_array()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| invalid("group must contain operations"))?;
            for id in ids {
                let id = id
                    .as_str()
                    .filter(|id| operations.contains_key(*id))
                    .ok_or_else(|| invalid("unknown group operation"))?;
                if !members.insert(id.to_owned()) {
                    return Err(invalid("overlapping or duplicate group operation"));
                }
                declared
                    .entry(name.to_owned())
                    .or_default()
                    .insert(id.to_owned());
            }
        }
    }
    if declared != annotated {
        return Err(invalid("access-groups disagree with operation annotations"));
    }
    if let Some(entry) = extension.get("entry") {
        let path = entry
            .as_str()
            .ok_or_else(|| invalid("entry must be a fixed GET path"))?;
        if paths.get(path).and_then(|p| p.get("get")).is_none() {
            return Err(invalid("entry must name a declared GET"));
        }
    }
    if let Some(changelog) = extension.get("changelog")
        && changelog.as_str().is_none_or(|s| s.trim().is_empty())
    {
        return Err(invalid(
            "changelog must explain changes and data compatibility",
        ));
    }
    Ok(())
}

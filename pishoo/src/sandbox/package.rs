//! Pishoo WEBC v3 profile: immutable container, publication signature and route binding.
// Protocol matches the independent store's WEBC package validation.
use std::collections::{BTreeMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Error, Result};
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

pub(crate) fn read_package(bytes: &[u8]) -> Result<webc::Container> {
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
        || manifest.atoms.len() > 1
        || manifest.atoms.keys().any(|name| name != "backend")
        || manifest.atoms.len() != container.atoms().len()
    {
        return Err(invalid(
            "package must contain one backend atom without commands, bindings or dependencies",
        ));
    }
    let metadata = package_metadata(&container)?;
    if metadata["kind"] == "app"
        && [
            "acl", "contact", "contacts", "sys", "pishoo", "file", "dhttp",
        ]
        .contains(&metadata["name"].as_str().unwrap())
    {
        return Err(invalid("App name conflicts with a native std service"));
    }
    let mappings = manifest
        .filesystem()
        .map_err(|_| invalid("invalid WEBC filesystem mappings"))?
        .ok_or_else(|| invalid("missing WEBC filesystem mappings"))?;
    let mount = if metadata["kind"] == "app" {
        "/public"
    } else {
        "/metadata"
    };
    if mappings.len() != 1
        || mappings[0].mount_path != mount
        || mappings[0].from.is_some()
        || mappings[0].volume_name == "pishoo-signature"
        || container
            .volumes()
            .keys()
            .any(|name| name != &mappings[0].volume_name && name != "pishoo-signature")
    {
        return Err(invalid(
            "require exactly the package type's local content mapping",
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
    if let Some(backend) = container.get_atom("backend") {
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
    }
    validate_content(&container, &metadata)?;
    if metadata["kind"] == "lib" {
        let raw = package_file(&container, "/metadata/openapi.json")?;
        super::validate_openapi(&raw)?;
        let document: Value =
            serde_json::from_slice(&raw).map_err(|_| invalid("invalid OpenAPI JSON"))?;
        if document["info"]["version"] != metadata["version"] {
            return Err(invalid("OpenAPI version differs from package version"));
        }
        validate_declarations(&document)?;
        validate_imports(&container.get_atom("backend").unwrap(), &metadata["host"])?;
    }
    verify_signature(bytes, &container, &metadata)?;
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
    let publisher = document["publisher"].as_str().unwrap();
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

/// Derive package metadata from the signed manifest; never fall back to OpenAPI.
pub(crate) fn package_metadata(container: &webc::Container) -> Result<Value> {
    let manifest = serde_json::to_value(container.manifest())
        .map_err(|_| invalid("invalid package manifest"))?;
    let wapm = &manifest["package"]["wapm"];
    let annotation = &manifest["package"]["pishoo"];
    let id = text(wapm, "name")?;
    super::valid_id(id).map_err(|_| invalid("invalid full package identity"))?;
    let (publisher, name) = id
        .split_once('/')
        .ok_or_else(|| invalid("package requires publisher/name"))?;
    if dhttp_home::normalize_name(publisher).as_deref() != Some(publisher) {
        return Err(invalid("publisher must be normalized"));
    }
    let version = text(wapm, "version")?;
    if !release_version(version) {
        return Err(invalid("version must be MAJOR.MINOR.PATCH"));
    }
    let kind = text(annotation, "kind")?;
    if !matches!(kind, "app" | "lib") {
        return Err(invalid("kind must be app or lib"));
    }
    let allowed = [
        "kind",
        "title",
        "dependencies",
        "changelog",
        "entry",
        "host",
    ];
    if annotation
        .as_object()
        .is_none_or(|o| o.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        return Err(invalid("unknown package.pishoo field"));
    }
    let dependencies = annotation["dependencies"]
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or_else(|| invalid("dependencies requires an explicit array with at most 100 Libs"))?;
    let mut ids = HashSet::new();
    for dep in dependencies {
        let publisher = text(dep, "publisher")?;
        let name = text(dep, "name")?;
        let id_dep = format!("{publisher}/{name}");
        if dep.as_object().is_none_or(|o| o.len() != 3)
            || super::valid_id(&id_dep).is_err()
            || dhttp_home::normalize_name(publisher).as_deref() != Some(publisher)
            || !release_version(text(dep, "version")?)
            || !ids.insert(id_dep.clone())
            || (kind == "lib" && id_dep == id)
        {
            return Err(invalid("invalid, duplicate or self Lib dependency"));
        }
    }
    let mut value = serde_json::json!({"id":id,"publisher":publisher,"name":name,"version":version,
        "kind":kind,"title":text(annotation,"title")?,"description":text(wapm,"description")?,
        "license":text(wapm,"license")?,"changelog":text(annotation,"changelog")?,"dependencies":dependencies});
    if let Some(repository) = wapm.get("repository") {
        let raw = repository
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid("repository must be a nonempty HTTPS URL"))?;
        let url = url::Url::parse(raw).map_err(|_| invalid("invalid repository URL"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !raw
                .get(..8)
                .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
            || raw[8..]
                .split(['/', '?', '#'])
                .next()
                .is_none_or(str::is_empty)
            || !url.username().is_empty()
            || url.password().is_some()
            || raw[8..]
                .split(['/', '?', '#'])
                .next()
                .is_some_and(|s| s.contains('@'))
            || raw.chars().any(char::is_whitespace)
        {
            return Err(invalid(
                "repository must be an HTTPS URL with a host and without credentials",
            ));
        }
        value["repository"] = repository.clone();
    }
    if kind == "app" {
        if annotation["entry"] != "/" || annotation.get("host").is_some() {
            return Err(invalid("app requires entry / and forbids host"));
        }
        value["entry"] = "/".into();
    } else {
        if annotation.get("entry").is_some() {
            return Err(invalid("lib forbids entry"));
        }
        let host = annotation["host"]
            .as_object()
            .ok_or_else(|| invalid("lib requires explicit host purposes"))?;
        for (key, purpose) in host {
            if !matches!(
                key.as_str(),
                "private-storage" | "request-identity" | "signatures"
            ) || purpose.as_str().is_none_or(|s| s.trim().is_empty())
            {
                return Err(invalid("unknown or unexplained host capability"));
            }
        }
        value["host"] = annotation["host"].clone();
    }
    Ok(value)
}

fn release_version(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && part.parse::<u64>().is_ok()
                && (part.len() == 1 || !part.starts_with('0'))
        })
}

fn validate_content(container: &webc::Container, metadata: &Value) -> Result<()> {
    let app = metadata["kind"] == "app";
    if app != container.atoms().is_empty() {
        return Err(invalid("app requires zero atoms; lib requires one backend"));
    }
    let mappings = container
        .manifest()
        .filesystem()
        .map_err(|_| invalid("invalid mappings"))?
        .unwrap();
    let mapping = &mappings[0];
    let root = mapping.host_path.as_deref().unwrap_or("/");
    let mut count = 0;
    let files = volume_files(
        &container.get_volume(&mapping.volume_name).unwrap(),
        root,
        &mut count,
    )?;
    if app && !files.contains_key(&format!("{}/index.html", root.trim_end_matches('/'))) {
        return Err(invalid("app requires public/index.html"));
    }
    if app
        && files
            .keys()
            .any(|file| file.ends_with("/metadata/openapi.json"))
    {
        return Err(invalid("app cannot contain metadata/openapi.json"));
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
fn validate_declarations(document: &Value) -> Result<()> {
    let info = &document["info"];
    text(info, "title")?;
    let extension = &document["x-pishoo"];
    if extension.get("templates").is_some() {
        return Err(invalid("external templates are not part of store v1"));
    }
    if extension
        .as_object()
        .is_some_and(|o| o.keys().any(|k| k != "access-groups"))
    {
        return Err(invalid("OpenAPI cannot declare package metadata"));
    }
    let paths = document["paths"]
        .as_object()
        .ok_or_else(|| invalid("paths must be an object"))?;
    let mut operations = BTreeMap::new();
    let mut annotated = BTreeMap::<String, HashSet<String>>::new();
    for (path, item) in paths {
        if ["/api", "/app"].iter().any(|mount| {
            path == mount
                || path
                    .strip_prefix(mount)
                    .is_some_and(|tail| tail.starts_with('/'))
        }) {
            return Err(invalid(
                "business API cannot contain host mounting prefixes",
            ));
        }
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
            if op_extension.as_object().is_some_and(|o| {
                o.keys()
                    .any(|key| !matches!(key.as_str(), "access-group" | "initial-access"))
            }) {
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
    Ok(())
}

fn validate_imports(bytes: &[u8], host: &Value) -> Result<()> {
    let host = host.as_object().unwrap();
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
    Ok(())
}

//! Standard ZIP release packages, signature verification and direct file access.
// Protocol matches the independent store's ZIP package validation.
use std::collections::{BTreeMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

use crate::{Error, Result};
pub(crate) type PackageFiles = BTreeMap<String, Vec<u8>>;
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
pub(crate) fn read_package(bytes: &[u8]) -> Result<PackageFiles> {
    let files = waz_read(bytes).map_err(|error| invalid(&error.to_string()))?;
    let metadata = package_metadata(&files)?;
    if metadata["name"] == "std" {
        return Err(invalid(
            "package name conflicts with the native std namespace",
        ));
    }
    if metadata["name"] == "app"
        && metadata["has_lib"] == true
        && metadata["publisher"] != "std.dhttp.net"
    {
        return Err(invalid(
            "ordinary Lib name conflicts with the frontend app namespace",
        ));
    }
    if let Some(backend) = files.get("backend.wasm") {
        if backend.get(..8) != Some(b"\0asm\x0d\0\x01\0") {
            return Err(invalid("backend must be a WASM component"));
        }
        for item in wasmparser::Parser::new(0).parse_all(&backend) {
            if let wasmparser::Payload::CustomSection(section) =
                item.map_err(|_| invalid("malformed backend"))?
                && section.name() == "pishoo:signature"
            {
                return Err(invalid(
                    "backend must not embed a second publisher signature",
                ));
            }
        }
    }

    if files.get("backend.wasm").is_some() {
        let raw = package_openapi(&files)?;
        let api = super::validate_openapi(raw)?;
        if metadata["publisher"] == "std.dhttp.net"
            && api.paths.as_ref().unwrap().keys().any(|path| {
                crate::routes::reserved(path)
                    || [
                        "/acl",
                        "/contact",
                        "/contacts",
                        "/sys",
                        "/pishoo",
                        "/file",
                        "/dhttp",
                    ]
                    .iter()
                    .any(|prefix| {
                        path == prefix
                            || path
                                .strip_prefix(prefix)
                                .is_some_and(|tail| tail.starts_with('/'))
                    })
            })
        {
            return Err(invalid("standard Lib conflicts with a reserved API path"));
        }
        if metadata["publisher"] == "std.dhttp.net" {
            let prefix = format!("/{}", metadata["name"].as_str().unwrap());
            if api.paths.as_ref().unwrap().keys().any(|path| {
                path != &prefix
                    && !path
                        .strip_prefix(&prefix)
                        .is_some_and(|tail| tail.starts_with('/'))
            }) {
                return Err(invalid(
                    "standard Lib paths must stay under its package name",
                ));
            }
        }
        let document: Value =
            serde_json::from_slice(raw).map_err(|_| invalid("invalid OpenAPI JSON"))?;
        if document["info"]["version"] != metadata["version"] {
            return Err(invalid("OpenAPI version differs from package version"));
        }
        validate_declarations(&document)?;
        let groups = document
            .get("x-access-groups")
            .or_else(|| document["x-pishoo"].get("access-groups"));
        if metadata["has_app"] != true
            && groups
                .and_then(Value::as_object)
                .is_some_and(|groups| groups.values().any(|group| group["app"] == true))
        {
            return Err(invalid(
                "app capability requires this package's public resources",
            ));
        }
    }
    verify_signature(&files, &metadata)?;
    Ok(files)
}

pub(crate) fn package_openapi(files: &PackageFiles) -> Result<&[u8]> {
    let backend = files
        .get("backend.wasm")
        .ok_or_else(|| invalid("package has no backend"))?;
    let mut document = None;
    let mut depth = 0;
    for item in wasmparser::Parser::new(0).parse_all(backend) {
        match item.map_err(|_| invalid("malformed backend"))? {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            wasmparser::Payload::CustomSection(section) if section.name() == "pishoo:openapi" => {
                if depth != 0 || document.is_some() || section.data().len() > 1024 * 1024 {
                    return Err(invalid(
                        "OpenAPI must be a unique bounded top-level section",
                    ));
                }
                document = Some(section.data());
            }
            _ => {}
        }
    }
    document.ok_or_else(|| invalid("missing embedded OpenAPI"))
}

fn verify_signature(files: &PackageFiles, document: &Value) -> Result<()> {
    let data = files
        .get("signature.json")
        .filter(|data| data.len() <= 64 * 1024)
        .ok_or_else(|| invalid("missing or oversized publisher signature"))?;
    let envelope: Value =
        serde_json::from_slice(data).map_err(|_| invalid("invalid signature envelope"))?;
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
    let payload = waz_payload(files);
    let mut input = b"pishoo-waz-signature-v1\0".to_vec();
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

/// Derive package metadata only from the signed JSON manifest.
pub(crate) fn package_metadata(files: &PackageFiles) -> Result<Value> {
    let manifest: Value = serde_json::from_slice(
        files
            .get("manifest.json")
            .ok_or_else(|| invalid("missing manifest.json"))?,
    )
    .map_err(|_| invalid("invalid package manifest"))?;
    if manifest["format"] != "waz/1"
        || manifest.as_object().is_none_or(|v| {
            v.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "format" | "package" | "dependencies" | "backend" | "frontend"
                )
            })
        })
    {
        return Err(invalid("unsupported WAZ manifest format or field"));
    }
    let package = &manifest["package"];
    if package.as_object().is_none_or(|v| {
        v.keys().any(|key| {
            !matches!(
                key.as_str(),
                "author"
                    | "name"
                    | "version"
                    | "title"
                    | "description"
                    | "license"
                    | "release-notes"
                    | "repository"
            )
        })
    }) {
        return Err(invalid("unknown WAZ package field"));
    }
    let publisher = text(package, "author")?;
    let name = text(package, "name")?;
    let id = format!("{publisher}/{name}");
    super::valid_id(&id).map_err(|_| invalid("invalid full package identity"))?;
    if dhttp_home::normalize_name(publisher).as_deref() != Some(publisher) {
        return Err(invalid("author must be normalized"));
    }
    let version = text(package, "version")?;
    if !release_version(version) {
        return Err(invalid("version must be MAJOR.MINOR.PATCH"));
    }
    let dependencies = manifest["dependencies"]
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or_else(|| {
            invalid("dependencies requires an explicit array with at most 100 packages")
        })?;
    let mut ids = HashSet::new();
    let mut dependencies_out = Vec::new();
    for dependency in dependencies {
        let author = text(dependency, "author")?;
        let dep_name = text(dependency, "name")?;
        let dep_version = text(dependency, "version")?;
        let dep_id = format!("{author}/{dep_name}");
        if dependency.as_object().is_none_or(|o| o.len() != 3)
            || super::valid_id(&dep_id).is_err()
            || dhttp_home::normalize_name(author).as_deref() != Some(author)
            || !release_version(dep_version)
            || !ids.insert(dep_id.clone())
            || dep_id == id
        {
            return Err(invalid("invalid, duplicate or self package dependency"));
        }
        dependencies_out
            .push(serde_json::json!({"publisher":author,"name":dep_name,"version":dep_version}));
    }
    let has_lib = manifest.get("backend").is_some();
    let has_app = manifest.get("frontend").is_some();
    if (has_lib && manifest["backend"] != "backend.wasm")
        || (has_app && manifest["frontend"] != "public")
        || has_lib != files.contains_key("backend.wasm")
        || has_app != files.keys().any(|name| name.starts_with("public/"))
        || (has_app && !files.contains_key("public/index.html"))
        || (!has_lib && !has_app)
    {
        return Err(invalid(
            "WAZ manifest runtime paths disagree with package files",
        ));
    }
    let mut value = serde_json::json!({"id":id,"publisher":publisher,"name":name,"version":version,
        "title":text(package,"title")?,"description":text(package,"description")?,"license":text(package,"license")?,
        "changelog":text(package,"release-notes")?,"dependencies":dependencies_out,"has_lib":has_lib,"has_app":has_app});
    if has_app {
        value["entry"] = "/".into();
    }
    if let Some(repository) = package.get("repository") {
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
    let groups = match (
        document.get("x-access-groups"),
        extension.get("access-groups"),
    ) {
        (Some(_), Some(_)) => return Err(invalid("duplicate access-groups declarations")),
        (groups, legacy) => groups.or(legacy),
    };
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
            let group = match (
                operation.get("x-access-group"),
                op_extension.get("access-group"),
            ) {
                (Some(_), Some(_)) => return Err(invalid("duplicate access-group declarations")),
                (group, legacy) => group.or(legacy),
            };
            if let Some(group) = group {
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
    let mut app_group = false;
    if let Some(groups) = groups {
        for (name, group) in groups
            .as_object()
            .ok_or_else(|| invalid("access-groups must be an object"))?
        {
            if !valid_name(name) {
                return Err(invalid("invalid group name"));
            }
            if let Some(titles) = group["title"].as_object() {
                if titles.len() != 2 || !titles.contains_key("en") || !titles.contains_key("zh-CN")
                {
                    return Err(invalid("localized group title requires en and zh-CN"));
                }
                text(&group["title"], "en")?;
                text(&group["title"], "zh-CN")?;
            } else {
                text(group, "title")?;
            }
            if group.get("app").is_some_and(|app| !app.is_boolean()) {
                return Err(invalid("group app must be a boolean"));
            }
            if group["app"] == true {
                if app_group {
                    return Err(invalid("app entry belongs to multiple groups"));
                }
                app_group = true;
            }
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

// WAZ v1 boundary codec, kept in sync with Jade archive.rs.

type WazResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const WAZ_LIMIT: usize = 64 * 1024 * 1024;

fn waz_is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

fn waz_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains(['\\', ':', '?', '#', '%'])
        && !path.chars().any(|c| c.is_control() || c.is_whitespace())
        && path.split('/').count() <= 33
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && (matches!(
            path,
            "manifest.json" | "signature.json" | "backend.wasm" | "README.md" | "CHANGELOG.md"
        ) || path.starts_with("public/"))
}

fn waz_read(bytes: &[u8]) -> WazResult<std::collections::BTreeMap<String, Vec<u8>>> {
    use std::io::Read;

    if bytes.len() > WAZ_LIMIT || !waz_is_zip(bytes) || bytes.len() < 22 {
        return Err("expected a bounded standard ZIP package".into());
    }
    // This profile uses ordinary single-disk ZIP, without ZIP64, comments,
    // self-extracting prefixes or trailing data. Every byte belongs to the archive.
    let end = bytes.len() - 22;
    let eocd = &bytes[end..];
    let u16_at = |offset: usize| u16::from_le_bytes(eocd[offset..offset + 2].try_into().unwrap());
    let u32_at =
        |offset: usize| u32::from_le_bytes(eocd[offset..offset + 4].try_into().unwrap()) as usize;
    let count = u16_at(10) as usize;
    let directory_size = u32_at(12);
    let directory_start = u32_at(16);
    if eocd[..4] != *b"PK\x05\x06"
        || u16_at(4) != 0
        || u16_at(6) != 0
        || u16_at(20) != 0
        || u16_at(8) as usize != count
        || count == 0
        || count > 4096
        || directory_start.checked_add(directory_size) != Some(end)
    {
        return Err("invalid ZIP end record or entry budget".into());
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    if archive.len() != count || archive.offset() != 0 {
        return Err("ZIP central directory disagrees with archive".into());
    }
    let mut files = std::collections::BTreeMap::new();
    let mut spans = std::collections::BTreeMap::new();
    let mut central_spans = std::collections::BTreeMap::new();
    let mut remaining = WAZ_LIMIT;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = std::str::from_utf8(entry.name_raw())?.to_owned();
        if !waz_path(&name)
            || entry.is_dir()
            || entry.encrypted()
            || !matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            )
            || entry
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000))
            || files.contains_key(&name)
        {
            return Err(format!("invalid, duplicate or unsupported ZIP entry: {name}").into());
        }
        let limit = match name.as_str() {
            "manifest.json" => 1024 * 1024,
            "signature.json" => 64 * 1024,
            "backend.wasm" => WAZ_LIMIT,
            _ => 16 * 1024 * 1024,
        }
        .min(remaining);
        let size = usize::try_from(entry.size())?;
        if size > limit {
            return Err(format!("ZIP entry exceeds its decompressed size limit: {name}").into());
        }
        let start = usize::try_from(entry.header_start())?;
        let data = usize::try_from(entry.data_start())?;
        let stop = data
            .checked_add(usize::try_from(entry.compressed_size())?)
            .ok_or("ZIP span overflow")?;
        let header = bytes
            .get(start..start.checked_add(30).ok_or("ZIP header overflow")?)
            .ok_or("truncated ZIP local header")?;
        let name_length = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
        let extra_length = u16::from_le_bytes(header[28..30].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(header[6..8].try_into().unwrap());
        let central_start = usize::try_from(entry.central_header_start())?;
        let central = bytes
            .get(
                central_start
                    ..central_start
                        .checked_add(46)
                        .ok_or("ZIP directory overflow")?,
            )
            .ok_or("truncated ZIP central header")?;
        let central_length = 46
            + u16::from_le_bytes(central[28..30].try_into().unwrap()) as usize
            + u16::from_le_bytes(central[30..32].try_into().unwrap()) as usize
            + u16::from_le_bytes(central[32..34].try_into().unwrap()) as usize;
        let central_end = central_start
            .checked_add(central_length)
            .ok_or("ZIP directory span overflow")?;
        if central[..4] != *b"PK\x01\x02"
            || central_start < directory_start
            || central_end > end
            || u16::from_le_bytes(central[8..10].try_into().unwrap()) != flags
            || central_spans.insert(central_start, central_end).is_some()
        {
            return Err("invalid ZIP central header".into());
        }
        if header[..4] != *b"PK\x03\x04"
            || flags & !0x0806 != 0 // UTF-8 and Deflate level hints only; no encryption/descriptors.
            || bytes.get(start + 30..start + 30 + name_length) != Some(name.as_bytes())
            || start + 30 + name_length + extra_length != data
            || u16::from_le_bytes(header[8..10].try_into().unwrap()) != if entry.compression() == zip::CompressionMethod::Stored { 0 } else { 8 }
            || u32::from_le_bytes(header[14..18].try_into().unwrap()) != entry.crc32()
            || u32::from_le_bytes(header[18..22].try_into().unwrap()) as u64 != entry.compressed_size()
            || u32::from_le_bytes(header[22..26].try_into().unwrap()) as u64 != entry.size()
            || stop > directory_start || spans.insert(start, stop).is_some()
        {
            return Err("ZIP local header disagrees with the central directory".into());
        }
        let mut content = Vec::new();
        entry
            .by_ref()
            .take(size as u64 + 1)
            .read_to_end(&mut content)?;
        if content.len() != size {
            return Err("ZIP decompressed size disagrees with header".into());
        }
        remaining -= content.len();
        files.insert(name, content);
    }
    let mut cursor = 0;
    for (start, stop) in spans {
        if start != cursor {
            return Err("overlapping or unindexed ZIP data".into());
        }
        cursor = stop;
    }
    if cursor != directory_start {
        return Err("unindexed ZIP data before directory".into());
    }
    for (start, stop) in central_spans {
        if start != cursor {
            return Err("overlapping or unindexed ZIP directory".into());
        }
        cursor = stop;
    }
    if cursor != end {
        return Err("unindexed ZIP directory data".into());
    }
    let mut directories = HashSet::new();
    for name in files.keys() {
        let mut ancestor = name.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if files.contains_key(parent) {
                return Err("ZIP file conflicts with a directory".into());
            }
            directories.insert(parent);
            if directories.len() + files.len() > 10000 {
                return Err("ZIP expanded directory entry budget exceeded".into());
            }
            ancestor = parent;
        }
    }
    if !files.contains_key("manifest.json") {
        return Err("ZIP package requires manifest.json".into());
    }
    Ok(files)
}

fn waz_payload(files: &std::collections::BTreeMap<String, Vec<u8>>) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"pishoo-waz-payload-v1\0");
    for (name, bytes) in files
        .iter()
        .filter(|(name, _)| name.as_str() != "signature.json")
    {
        digest.update((name.len() as u32).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(Sha256::digest(bytes));
    }
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
pub(super) fn waz_write(files: &std::collections::BTreeMap<String, Vec<u8>>) -> WazResult<Vec<u8>> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6))
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    for (name, bytes) in files {
        writer.start_file(name, options)?;
        writer.write_all(bytes)?;
    }
    let bytes = writer.finish()?.into_inner();
    waz_read(&bytes)?;
    Ok(bytes)
}

// Runtime fixtures extract the same raw component as installed packages.
#[cfg(test)]
pub(crate) fn runtime_fixture(bytes: &[u8]) -> Vec<u8> {
    waz_read(bytes).unwrap().remove("backend.wasm").unwrap()
}

#[cfg(test)]
mod tests;

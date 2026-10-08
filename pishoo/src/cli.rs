//! Local resource commands and explicitly requested system service operations.
use std::{ffi::OsString, io::Read};

use http::{Method, Uri};
use serde_json::{Value, json};

use crate::{
    Error, Result,
    sandbox::{WasmRuntime, check_lib, install_lib, installed_libs, remove_lib},
    setup::config_database,
};

pub async fn run_command(args: Vec<OsString>) -> Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h")
        || args.first().is_some_and(|arg| arg == "help")
    {
        println!(
            "\
Usage:
  pishoo
  pishoo <command> [arguments] [--id NAME]

Configuration:
  listen [SCOPE]               Show or update the saved listening scope
  proxy [LOCATION [TARGET]]    List, inspect, or update proxy rules
  proxy remove|rm LOCATION     Remove a proxy rule
  proxy clear                  Remove all proxy rules
  proxy replace FILE.json      Replace all proxy rules

Components:
  lib [list|ls]                List installed Libs on disk
  lib --loaded                 Query Libs loaded by the running service
  lib info ID                  Inspect an installed Lib
  lib install ID FILE.wasm     Install or update a Lib
  lib remove|rm ID             Remove a Lib; preserve its data
  lib check FILE.wasm          Validate a component without selecting an identity

Service:
  start                        Start the installed Pishoo service
  stop                         Stop the installed Pishoo service
  restart                      Restart the installed Pishoo service
  status                       Show the service manager status

Options:
  -i, --id NAME                Select an identity for resource commands
  -h, --help                   Show this help
  -V, --version                Show the version

Listening scopes: off (0), internal (1), external (2), both (3).
Proxy listing also accepts list or ls.
Identity selection may appear before or after a resource command.
The default identity is read from settings.toml [default].name in DHTTP_HOME
(default: ~/.dhttp). Service commands and lib check do not accept --id.

With no command, Pishoo runs all identities in the foreground.
Resource changes are saved to disk and take effect after restarting Pishoo.
Queries are written to stdout; context and status messages to stderr."
        );
        return Ok(());
    }
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("pishoo {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut name = None;
    let mut positional = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--id" || arg == "-i" {
            if name.is_some() {
                return Err(usage("identity specified more than once"));
            }
            name = Some(args.next().ok_or_else(|| usage("--id requires NAME"))?);
        } else if let Some(value) = arg.to_str().and_then(|s| s.strip_prefix("--id=")) {
            if name.is_some() || value.is_empty() {
                return Err(usage("invalid --id"));
            }
            name = Some(value.into());
        } else {
            positional.push(arg);
        }
    }
    let command = positional
        .first()
        .and_then(|s| s.to_str())
        .ok_or_else(|| usage("expected a command; use --help"))?
        .to_owned();
    let args = &positional[1..];
    if matches!(command.as_str(), "start" | "stop" | "restart" | "status") {
        if name.is_some() || !args.is_empty() {
            return Err(usage("service commands accept no identity or arguments"));
        }
        return tokio::task::spawn_blocking(move || service_command(&command)).await?;
    }
    if command == "lib" && args.first().is_some_and(|s| s == "check") {
        if name.is_some() || args.len() != 2 {
            return Err(usage("lib check FILE.wasm does not accept --id"));
        }
        let path = std::path::PathBuf::from(&args[1]);
        let value = tokio::task::spawn_blocking(move || {
            let bytes = read_input(&path, 64 * 1024 * 1024)?;
            let api = check_lib(&bytes, &WasmRuntime::new()?)?;
            Ok::<_, Error>(lib_metadata(&api, None))
        })
        .await??;
        return print_json(&value);
    }
    // Parse once before resolving the home; replacement JSON is retained locally.
    let proxy = if command == "proxy" {
        Some(proxy_operation(args)?)
    } else {
        None
    };
    match command.as_str() {
        "listen" if args.len() <= 1 => {
            if let Some(value) = args.first() {
                listen_value(value)?;
            }
        }
        "proxy" => {}
        "lib" => {
            lib_operation(args)?;
        }
        _ => return Err(usage("invalid command; use --help")),
    }
    let home =
        dhttp_home::DhttpHome::load(dhttp_home::HomeScope::User).map_err(std::io::Error::other)?;
    let profile = tokio::task::spawn_blocking(move || select_profile(&home, name)).await??;
    eprintln!(
        "Identity:   {}\nDirectory:  {}",
        profile.name(),
        profile.path().display()
    );
    if command == "lib" && args.first().is_some_and(|s| s == "--loaded") {
        let endpoint = dhttp::Endpoint::load(profile.name()).await?;
        let mdns = crate::dns::install()?;
        let result = async {
            dhttp::DhttpNetwork::init().await?;
            crate::dns::maintain_mdns(&mdns, &[], &[]).await?;
            let uri: Uri = format!("https://{}/workspace-api/libs", profile.name())
                .parse()
                .map_err(|_| usage("invalid identity URI"))?;
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(30), endpoint.get(uri))
                    .await
                    .map_err(|_| Error::Deadline)??;
            let status = response.status();
            let bytes = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                axum::body::to_bytes(
                    axum::body::Body::new(response.into_body()),
                    64 * 1024 * 1024,
                ),
            )
            .await
            .map_err(|_| Error::Deadline)?
            .map_err(std::io::Error::other)?;
            if !status.is_success() {
                return Err(Error::Io(std::io::Error::other(format!(
                    "loaded Lib query returned {status}"
                ))));
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            if !value.is_array() {
                return Err(Error::Io(std::io::Error::other(
                    "invalid loaded Lib catalog",
                )));
            }
            eprintln!("Catalog:    loaded (running service)");
            print_json(&value)
        }
        .await;
        mdns.shutdown().await.map_err(std::io::Error::other)?;
        return result;
    }
    let args = args.to_vec();
    let (value, saving) = tokio::task::spawn_blocking(move || -> Result<_> {
        match command.as_str() {
            "listen" => {
                let payload = args
                    .first()
                    .map(listen_value)
                    .transpose()?
                    .map(|listen| json!({"listen":listen}));
                let saving = payload.is_some();
                let value = config_database(
                    &profile,
                    if saving { &Method::PATCH } else { &Method::GET },
                    &Uri::from_static("/pishoo/settings"),
                    payload,
                )?;
                let listen = value["listen"].as_u64().unwrap();
                println!(
                    "Listen:     {} ({listen})",
                    ["off", "internal", "external", "both"][listen as usize]
                );
                Ok((None, saving))
            }
            "proxy" => {
                let (method, uri, payload) = proxy.unwrap();
                let saving = method != Method::GET;
                let value = config_database(&profile, &method, &uri, payload)?;
                Ok((if value.is_null() { None } else { Some(value) }, saving))
            }
            "lib" => {
                let operation = args.first().and_then(|s| s.to_str()).unwrap_or("list");
                let value = match operation {
                    "list" | "ls" => {
                        eprintln!("Catalog:    installed (disk)");
                        installed_libs(&profile, None)?
                    }
                    "info" => installed_libs(&profile, Some(text(&args[1])?))?,
                    "install" => {
                        let bytes = read_input(std::path::Path::new(&args[2]), 64 * 1024 * 1024)?;
                        install_lib(&profile, text(&args[1])?, &bytes, &WasmRuntime::new()?)?
                    }
                    "remove" | "rm" => {
                        remove_lib(&profile, text(&args[1])?)?;
                        Value::Null
                    }
                    _ => return Err(usage("invalid lib operation")),
                };
                Ok((
                    if value.is_null() { None } else { Some(value) },
                    matches!(operation, "install" | "remove" | "rm"),
                ))
            }
            _ => unreachable!(),
        }
    })
    .await??;
    if let Some(value) = value {
        print_json(&value)?;
    }
    if saving {
        eprintln!("Status:     Saved to disk. Restart Pishoo to apply changes.");
    }
    Ok(())
}

fn usage(message: &str) -> Error {
    Error::BadRequest(message.into())
}
fn text(arg: &OsString) -> Result<&str> {
    arg.to_str().ok_or_else(|| usage("expected UTF-8 argument"))
}
fn print_json(value: &Value) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value).map_err(std::io::Error::other)?;
    writeln!(stdout)?;
    Ok(())
}
fn read_input(path: &std::path::Path, limit: usize) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(usage("input must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(usage("input exceeds size limit"));
    }
    Ok(bytes)
}
fn listen_value(arg: &OsString) -> Result<u8> {
    match text(arg)? {
        "off" | "0" => Ok(0),
        "internal" | "1" => Ok(1),
        "external" | "2" => Ok(2),
        "both" | "3" => Ok(3),
        _ => Err(usage(
            "listen must be off, internal, external, both, or 0..3",
        )),
    }
}
fn select_profile(
    home: &dhttp_home::DhttpHome,
    name: Option<OsString>,
) -> Result<dhttp_home::identity::IdentityProfile> {
    let name = match name {
        Some(name) => text(&name)?.to_owned(),
        None => {
            let bytes = read_input(&home.join("settings.toml"), 64 * 1024).map_err(|error| {
                usage(&format!(
                    "cannot read default identity: {error}; use --id NAME"
                ))
            })?;
            let value: toml::Value = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| toml::from_str(s).ok())
                .ok_or_else(|| usage("cannot parse settings.toml; use --id NAME"))?;
            value
                .get("default")
                .and_then(|v| v.get("name"))
                .and_then(toml::Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| usage("default.name is missing; use --id NAME"))?
                .to_owned()
        }
    };
    let profile = home
        .identity_profile(&name)
        .map_err(|_| usage("invalid identity name; use --id NAME"))?;
    let metadata = profile.path().symlink_metadata().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            usage("selected identity does not exist; use --id NAME")
        } else {
            Error::Io(error)
        }
    })?;
    if !metadata.file_type().is_dir() {
        return Err(usage(
            "identity must be an existing real directory; use --id NAME",
        ));
    }
    Ok(profile)
}
fn proxy_operation(args: &[OsString]) -> Result<(Method, Uri, Option<Value>)> {
    if args.len() == 2 && args[0] == "replace" {
        let bytes = read_input(std::path::Path::new(&args[1]), 64 * 1024)?;
        let value = serde_json::from_slice(&bytes).map_err(|_| usage("invalid proxies JSON"))?;
        return Ok((
            Method::PUT,
            Uri::from_static("/pishoo/proxies"),
            Some(value),
        ));
    }
    let words = args.iter().map(text).collect::<Result<Vec<_>>>()?;
    let query = |location: &str| -> Result<Uri> {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("location", location)
            .finish();
        format!("/pishoo/proxies?{query}")
            .parse()
            .map_err(|_| usage("invalid proxy location"))
    };
    let uri = Uri::from_static("/pishoo/proxies");
    match words.as_slice() {
        [] | ["list" | "ls"] => Ok((Method::GET, uri, None)),
        ["clear"] => Ok((Method::PUT, uri, Some(json!([])))),
        ["remove" | "rm", location] if location.starts_with('/') || location.starts_with("= /") => {
            Ok((Method::DELETE, query(location)?, None))
        }
        [location] if location.starts_with('/') || location.starts_with("= /") => {
            Ok((Method::GET, query(location)?, None))
        }
        [location, upstream] if location.starts_with('/') || location.starts_with("= /") => Ok((
            Method::PATCH,
            uri,
            Some(json!({"location":location, "proxy_pass":upstream})),
        )),
        _ => Err(usage("invalid proxy arguments; use --help")),
    }
}
fn lib_operation(args: &[OsString]) -> Result<()> {
    match args.first().map(text).transpose()?.unwrap_or("list") {
        "list" | "ls" if args.len() <= 1 => return Ok(()),
        "--loaded" if args.len() == 1 => return Ok(()),
        "info" | "remove" | "rm" if args.len() == 2 => {}
        "install" if args.len() == 3 => {}
        _ => return Err(usage("invalid lib arguments; use --help")),
    }
    let id = text(&args[1])?;
    if id.len() > 63
        || !id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(usage("invalid Lib id"));
    }
    Ok(())
}

fn service_command(operation: &str) -> Result<()> {
    use std::process::Command;
    #[cfg(target_os = "linux")]
    {
        let loaded = Command::new("systemctl")
            .args(["show", "--property=LoadState", "--value", "pishoo.service"])
            .output()?;
        if !loaded.status.success() || String::from_utf8_lossy(&loaded.stdout).trim() == "not-found"
        {
            return Err(Error::Io(std::io::Error::other(
                "pishoo.service is not installed or cannot be queried",
            )));
        }
        let status = Command::new("systemctl")
            .args([operation, "pishoo.service"])
            .status()?;
        if status.success() || operation == "status" && status.code() == Some(3) {
            return Ok(());
        }
        return Err(Error::Io(std::io::Error::other(format!(
            "systemctl {operation} failed ({status})"
        ))));
    }
    #[cfg(target_os = "macos")]
    {
        let installed = Command::new("brew")
            .args(["list", "--formula", "pishoo"])
            .output()?;
        if !installed.status.success() {
            return Err(Error::Io(std::io::Error::other(
                "Homebrew pishoo is not installed or cannot be queried",
            )));
        }
        let mut command = Command::new("brew");
        command.arg("services");
        if operation == "status" {
            command.args(["info", "pishoo"]);
        } else {
            command.args([operation, "pishoo"]);
        }
        let status = command.status()?;
        if status.success() {
            return Ok(());
        }
        return Err(Error::Io(std::io::Error::other(format!(
            "brew services {operation} failed ({status})"
        ))));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "service management requires Linux/systemd or macOS/Homebrew",
    )))
}

fn lib_metadata(openapi: &oas3::OpenApiV3Spec, id: Option<&str>) -> serde_json::Value {
    let mut endpoints = Vec::new();
    if let Some(paths) = &openapi.paths {
        for (path, item) in paths {
            for (method, operation) in item.methods() {
                let description = operation
                    .summary
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .or_else(|| {
                        operation
                            .description
                            .as_deref()
                            .filter(|text| !text.trim().is_empty())
                    })
                    .map(str::to_owned)
                    .or_else(|| {
                        operation
                            .responses
                            .as_ref()?
                            .iter()
                            .find_map(|(status, response)| {
                                if status != "2XX"
                                    && !status
                                        .parse::<u16>()
                                        .is_ok_and(|code| (200..300).contains(&code))
                                {
                                    return None;
                                }
                                response
                                    .resolve(openapi)
                                    .ok()?
                                    .description
                                    .filter(|text| !text.trim().is_empty())
                            })
                    });
                endpoints.push(serde_json::json!({"method":method.as_str(), "path":id.map_or_else(|| path.clone(), |id| format!("/api/{id}{path}")), "description":description}));
            }
        }
    }
    endpoints.sort_by(|a, b| {
        a["path"]
            .as_str()
            .cmp(&b["path"].as_str())
            .then_with(|| a["method"].as_str().cmp(&b["method"].as_str()))
    });
    let mut value = serde_json::json!({"title":openapi.info.title, "version":openapi.info.version, "description":openapi.info.description, "endpoints":endpoints});
    if let Some(id) = id {
        value["id"] = id.into();
    }
    value
}

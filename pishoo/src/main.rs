use tracing_subscriber::{fmt::writer::MakeWriterExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if !args.is_empty() {
        if let Err(error) = run_command(args) {
            eprintln!("pishoo: error: {error}");
            std::process::exit(match error {
                pishoo::Error::BadRequest(_) | pishoo::Error::InvalidComponent(_) => 2,
                _ => 1,
            });
        }
        return;
    }
    #[cfg(target_os = "linux")]
    let multiuser = rustix::process::geteuid().is_root();
    #[cfg(not(target_os = "linux"))]
    let multiuser = false;
    let home = if multiuser {
        dhttp_home::DhttpHome::load(dhttp_home::HomeScope::Global).map_err(std::io::Error::other)
    } else {
        dirs::home_dir()
            .map(dhttp_home::DhttpHome::for_user_home_dir)
            .ok_or_else(|| std::io::Error::other("cannot locate user home directory"))
    };
    let subscriber = home.and_then(|home| logging(&home));
    match subscriber {
        Ok(subscriber) => subscriber.init(),
        Err(error) => {
            eprintln!("pishoo: cannot initialize error.log: {error}");
            std::process::exit(1);
        }
    }
    tracing::trace!(pid = std::process::id(), "Pishoo tracing initialized");
    if let Err(error) = pishoo::run().await {
        tracing::error!(%error, "Pishoo stopped");
        std::process::exit(1);
    }
}

fn logging(
    home: &dhttp_home::DhttpHome,
) -> std::io::Result<impl tracing::Subscriber + Send + Sync + use<>> {
    let directory = home.join("log");
    let legacy = home.join("logs");
    if !directory.try_exists()? && legacy.try_exists()? {
        if !legacy.symlink_metadata()?.file_type().is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "legacy log root must be a real directory",
            ));
        }
        std::fs::rename(legacy, &directory)?;
    }
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(&directory)?;
    let path = directory.join("error.log");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|error| {
        std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
    })?;
    // The existing subscriber serializes each event through the owned file.
    // Keep stderr available to the service manager and interactive runs.
    let writer = std::sync::Mutex::new(file).and(std::io::stderr);
    Ok(tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(writer)
        .finish())
}

fn run_command(args: Vec<std::ffi::OsString>) -> Result<(), pishoo::Error> {
    if args.first().is_some_and(|arg| arg == "configure") {
        let (path, mode, initialize) = match &args[1..] {
            [flag, path] if flag == "--profile" => (path, None, false),
            [flag, path, init] if flag == "--profile" && init == "--init" => (path, None, true),
            [flag, path, listen, mode] if flag == "--profile" && listen == "--listen" => {
                let mode = mode.to_str().filter(|mode| matches!(*mode, "internal" | "external" | "all" | "off"))
                    .ok_or_else(|| pishoo::Error::BadRequest("invalid listen mode".into()))?;
                (path, Some(mode), false)
            }
            _ => return Err(pishoo::Error::BadRequest("usage: pishoo configure --profile PATH [--init | --listen internal|external|all|off]".into())),
        };
        let path = std::path::PathBuf::from(path);
        if !path.is_absolute() {
            return Err(pishoo::Error::BadRequest(
                "profile path must be absolute".into(),
            ));
        }
        let profile = dhttp_home::identity::IdentityProfile::try_from(path)
            .map_err(|error| pishoo::Error::BadRequest(format!("invalid profile: {error}")))?;
        println!("{}", pishoo::configure(&profile, mode, initialize)?);
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "reload") {
        return reload(&args[1..]);
    }
    if args.len() == 1 {
        if args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
            println!(
                "Usage: pishoo [configure --profile PATH [--init | --listen MODE] | reload [--pid PID] | --help | --version]\n\nRun configured identities in the foreground. Legacy host configuration gets fresh v3 defaults; identities without host configuration are skipped.\n\nReload sends SIGHUP to the running service; use --pid for a foreground process. System services are managed with systemctl or brew services.\n\nManagement commands are provided by gmutils:\n  genmeta listen --id NAME [internal|external|all|off]\n  genmeta install --list --id NAME\n  genmeta install --info PACKAGE --id NAME\n  genmeta mount --id NAME [PATH URL]\n  genmeta unmount --id NAME PATH URL\n  genmeta --help"
            );
            return Ok(());
        }
        if args[0] == "--version" || args[0] == "-V" {
            println!("pishoo {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    }
    Err(pishoo::Error::BadRequest(
        "Pishoo management commands have moved to genmeta; use pishoo --help".into(),
    ))
}

fn reload(args: &[std::ffi::OsString]) -> Result<(), pishoo::Error> {
    use std::process::Command;
    let pid = match args {
        [flag, pid] if flag == "--pid" => pid.to_string_lossy().into_owned(),
        [] => {
            #[cfg(target_os = "linux")]
            {
                let output = Command::new("systemctl")
                    .args(["show", "--property=MainPID", "--value", "pishoo.service"])
                    .output()?;
                if !output.status.success() {
                    return Err(pishoo::Error::BadRequest("cannot query pishoo.service; use reload --pid PID for a foreground process".into()));
                }
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            }
            #[cfg(target_os = "macos")]
            {
                let domain = if rustix::process::getuid().is_root() {
                    "system".to_owned()
                } else {
                    format!("gui/{}", rustix::process::getuid().as_raw())
                };
                let output = Command::new("launchctl")
                    .args(["print", &format!("{domain}/homebrew.mxcl.pishoo")])
                    .output()?;
                if !output.status.success() {
                    return Err(pishoo::Error::BadRequest("cannot query Homebrew pishoo service; use reload --pid PID for a foreground process".into()));
                }
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("pid = ").map(str::to_owned))
                    .ok_or_else(|| {
                        pishoo::Error::BadRequest("Pishoo service is not running".into())
                    })?
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                return Err(pishoo::Error::BadRequest(
                    "reload requires a Unix process with SIGHUP support".into(),
                ));
            }
        }
        _ => {
            return Err(pishoo::Error::BadRequest(
                "usage: pishoo reload [--pid PID]".into(),
            ));
        }
    };
    let pid = pid
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
        .ok_or_else(|| {
            pishoo::Error::BadRequest("Pishoo is not running or PID is invalid".into())
        })?;
    let status = Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()?;
    if !status.success() {
        return Err(pishoo::Error::Io(std::io::Error::other(format!(
            "reload signal failed ({status})"
        ))));
    }
    eprintln!("Reload requested for PID {pid}; check the Pishoo log for configuration errors.");
    Ok(())
}

#[cfg(test)]
mod command_tests {
    use super::*;
    #[test]
    fn management_commands_are_rejected_before_any_side_effects() {
        for command in [
            "enable", "disable", "listen", "proxy", "lib", "app", "start", "stop", "restart",
            "status",
        ] {
            assert!(matches!(
                run_command(vec![command.into()]),
                Err(pishoo::Error::BadRequest(_))
            ));
        }
        assert!(run_command(vec!["lib".into(), "--help".into()]).is_err());
        assert!(run_command(vec!["--version".into()]).is_ok());
        assert!(run_command(vec!["--help".into()]).is_ok());
    }
}

#[cfg(test)]
#[path = "../tests/unit/logging.rs"]
mod tests;

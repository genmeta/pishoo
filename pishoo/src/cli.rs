//! The host runs services; management commands belong to gmutils.
use crate::{Error, Result};
use std::ffi::OsString;

pub async fn run_command(args: Vec<OsString>) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "reload") {
        return reload(&args[1..]);
    }
    if args.len() == 1 {
        if args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
            println!(
                "Usage: pishoo [reload [--pid PID] | --help | --version]\n\nRun configured identities in the foreground. Identities without db/config.db are skipped.\n\nReload sends SIGHUP to the running service; use --pid for a foreground process. System services are managed with systemctl or brew services.\n\nManagement commands are provided by gmutils:\n  genmeta identity ensite|dissite|listen --id NAME\n  genmeta server proxy --id NAME\n  genmeta package --help"
            );
            return Ok(());
        }
        if args[0] == "--version" || args[0] == "-V" {
            println!("pishoo {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    }
    Err(Error::BadRequest("Pishoo management commands have moved to genmeta identity, genmeta server and genmeta package; use pishoo --help".into()))
}

fn reload(args: &[OsString]) -> Result<()> {
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
                    return Err(Error::BadRequest("cannot query pishoo.service; use reload --pid PID for a foreground process".into()));
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
                    return Err(Error::BadRequest("cannot query Homebrew pishoo service; use reload --pid PID for a foreground process".into()));
                }
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("pid = ").map(str::to_owned))
                    .ok_or_else(|| Error::BadRequest("Pishoo service is not running".into()))?
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                return Err(Error::BadRequest(
                    "reload requires a Unix process with SIGHUP support".into(),
                ));
            }
        }
        _ => return Err(Error::BadRequest("usage: pishoo reload [--pid PID]".into())),
    };
    let pid = pid
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
        .ok_or_else(|| Error::BadRequest("Pishoo is not running or PID is invalid".into()))?;
    let status = Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()?;
    if !status.success() {
        return Err(Error::Io(std::io::Error::other(format!(
            "reload signal failed ({status})"
        ))));
    }
    eprintln!("Reload requested for PID {pid}; check the Pishoo log for configuration errors.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn management_commands_are_rejected_before_any_side_effects() {
        for command in [
            "enable", "disable", "listen", "proxy", "lib", "app", "start", "stop", "restart",
            "status",
        ] {
            assert!(matches!(
                run_command(vec![command.into()]).await,
                Err(Error::BadRequest(_))
            ));
        }
        assert!(
            run_command(vec!["lib".into(), "--help".into()])
                .await
                .is_err()
        );
        assert!(run_command(vec!["--version".into()]).await.is_ok());
        assert!(run_command(vec!["--help".into()]).await.is_ok());
    }
}

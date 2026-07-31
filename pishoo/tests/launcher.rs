use std::{
    io::Read,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nix::{sys::wait::WaitStatus, unistd::User};
use pishoo::hypervisor::launcher::launch_worker;

fn current_user() -> User {
    User::from_uid(nix::unistd::getuid())
        .expect("resolve current uid")
        .expect("current user exists")
}

fn unique_home_dir(prefix: &str) -> std::path::PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{stamp}", std::process::id()))
}

#[tokio::test(flavor = "current_thread")]
async fn unix_launcher_socketpair_and_handle_lifecycle() {
    let user = current_user();
    let home = unique_home_dir("pishoo-launcher-mux");
    std::fs::create_dir_all(&home).expect("create temp home");

    let launched = launch_worker(Path::new("/bin/cat"), user.uid, user.gid, &user.name, &home)
        .expect("launch worker");

    let mut handle = launched.handle;

    // Verify that mux_fd is a valid, connected UNIX stream socket.
    nix::fcntl::fcntl(&launched.mux_fd, nix::fcntl::FcntlArg::F_GETFD)
        .expect("mux_fd should be a valid file descriptor");

    handle.start_kill().expect("kill worker");
    for _ in 0..20 {
        if handle.try_wait().expect("poll worker").is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("worker did not exit after kill");
}

#[tokio::test(flavor = "current_thread")]
async fn unix_launcher_sets_explicit_exec_environment() {
    let user = current_user();
    let home = unique_home_dir("pishoo-launcher-env");
    std::fs::create_dir_all(&home).expect("create temp home");
    let worker = home.join("print-environment");
    std::fs::write(
        &worker,
        "#!/bin/sh\nprintf '%s\\n' \"$HOME\" \"$USER\" \"$LOGNAME\" \"$PATH\" \"$PISHOO_USER\" >&3\n",
    )
    .expect("write environment probe");
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700))
        .expect("make environment probe executable");

    let launched =
        launch_worker(&worker, user.uid, user.gid, &user.name, &home).expect("launch worker");
    let mut handle = launched.handle;
    let mut ipc = std::os::unix::net::UnixStream::from(launched.mux_fd);
    let mut output = String::new();
    ipc.read_to_string(&mut output)
        .expect("read environment probe output");

    let mut exit_status = None;
    for _ in 0..20 {
        if let Some(status) = handle.try_wait().expect("poll env worker") {
            exit_status = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let status = exit_status.expect("env worker exited");
    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        "environment probe must exit successfully"
    );

    let values = output.lines().collect::<Vec<_>>();
    assert_eq!(
        values,
        [
            home.to_str().expect("temporary home is valid UTF-8"),
            user.name.as_str(),
            user.name.as_str(),
            std::env::var("PATH").as_deref().unwrap_or("/usr/bin:/bin"),
            user.name.as_str(),
        ]
    );
}

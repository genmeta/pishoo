use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nix::{sys::wait::WaitStatus, unistd::User};
use pishoo::launcher::launch_worker;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
async fn unix_launcher_wires_stdio_and_handle_lifecycle() {
    let user = current_user();
    let home = unique_home_dir("pishoo-launcher-stdio");
    std::fs::create_dir_all(&home).expect("create temp home");

    let launched = launch_worker(Path::new("/bin/cat"), user.uid, user.gid, &user.name, &home)
        .expect("launch worker");

    let mut handle = launched.handle;
    let mut transport = launched.transport;
    transport
        .stdin
        .write_all(b"ping-through-launcher\n")
        .await
        .expect("write stdin");

    let mut echoed = vec![0_u8; "ping-through-launcher\n".len()];
    transport
        .stdout
        .read_exact(&mut echoed)
        .await
        .expect("read stdout");
    assert_eq!(echoed, b"ping-through-launcher\n");

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

    let launched = launch_worker(
        Path::new("/usr/bin/env"),
        user.uid,
        user.gid,
        &user.name,
        &home,
    )
    .expect("launch worker");

    let mut handle = launched.handle;
    let mut stdout = launched.transport.stdout;
    let mut output = Vec::new();
    stdout
        .read_to_end(&mut output)
        .await
        .expect("read env output");
    let status = handle
        .try_wait()
        .expect("poll env worker")
        .expect("env worker exited");
    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        "env worker must exit successfully"
    );

    let output = String::from_utf8(output).expect("utf8 env output");
    assert!(output.contains(&format!("HOME={}", home.display())));
    assert!(output.contains(&format!("USER={}", user.name)));
    assert!(output.contains(&format!("LOGNAME={}", user.name)));
    assert!(output.contains("PATH="));
}

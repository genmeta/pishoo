use std::{
    ffi::{CStr, CString},
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
    path::Path,
};

use nix::{
    errno::Errno,
    unistd::{
        ForkResult, Gid, SysconfVar, Uid, execve, fork, getegid, geteuid, getgid, getuid, setgid,
        setuid, sysconf,
    },
};
use snafu::{ResultExt, Snafu};

use crate::hypervisor::worker_handle::WorkerHandle;

pub(crate) const CHILD_IPC_FD: RawFd = 3;

pub struct LaunchedWorker {
    pub handle: WorkerHandle,
    /// Root-side end of the MuxChannel socketpair for IPC with the worker.
    pub mux_fd: OwnedFd,
}

/// Transport for a launched session child process (socketpair for MuxChannel).
#[cfg(feature = "sshd")]
pub struct SessionTransport {
    /// Root-side end of the socketpair. The worker will receive this FD via
    /// MuxChannel FD passing, then establish a MuxChannel to the child.
    pub mux_fd: OwnedFd,
    pub child_pid: nix::unistd::Pid,
}

struct ChildExecSpec<'a> {
    worker_bin: &'a CString,
    argv: &'a [CString],
    envp: &'a [CString],
    uid: Uid,
    gid: Gid,
    username: &'a CString,
    credential_setup: ChildCredentialSetup,
    /// Socketpair FD to dup2 to FD 3 (MuxChannel IPC).
    mux_fd: &'a OwnedFd,
    max_fd: i32,
}

pub(crate) fn install_child_ipc_fd<Fd: AsFd>(source: Fd) -> std::io::Result<()> {
    let source_fd = source.as_fd().as_raw_fd();

    if source_fd != CHILD_IPC_FD {
        let rc = unsafe { libc::dup2(source_fd, CHILD_IPC_FD) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    let flags = unsafe { libc::fcntl(CHILD_IPC_FD, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let rc = unsafe { libc::fcntl(CHILD_IPC_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

#[derive(Debug, Snafu)]
pub enum LaunchWorkerError {
    #[snafu(display("worker username `{username}` contains nul byte"))]
    InvalidUsername {
        username: String,
        source: std::ffi::NulError,
    },
    #[snafu(display("root privileges are required to launch worker `{username}`"))]
    RootRequired {
        username: String,
        uid: u32,
        gid: u32,
        current_uid: u32,
        current_gid: u32,
    },
    #[snafu(display("worker path contains nul byte"))]
    InvalidWorkerPath { source: std::ffi::NulError },
    #[snafu(display("failed to build exec environment for user `{username}`"))]
    BuildExecEnv {
        username: String,
        source: BuildExecEnvError,
    },
    #[snafu(display("failed to create worker socketpair"))]
    CreateSocketpair { source: std::io::Error },
    #[snafu(display("failed to fork worker process"))]
    ForkWorker { source: Errno },
}

/// Errors from [`launch_session`].
#[cfg(feature = "sshd")]
#[derive(Debug, Snafu)]
pub enum LaunchSessionError {
    #[snafu(display("session binary path contains nul byte"))]
    InvalidSessionPath { source: std::ffi::NulError },
    #[snafu(display("failed to build exec environment for user `{username}`"))]
    BuildSessionExecEnv {
        username: String,
        source: BuildExecEnvError,
    },
    #[snafu(display("failed to create session socketpair"))]
    CreateSessionSocketpair { source: std::io::Error },
    #[snafu(display("failed to fork session process"))]
    ForkSession { source: Errno },
}

#[derive(Debug, Snafu)]
pub enum BuildExecEnvError {
    #[snafu(display("exec environment contains nul byte"))]
    EntryContainsNul { source: std::ffi::NulError },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildCredentialSetup {
    DropFromRoot,
    AlreadyTarget,
}

fn plan_child_credentials(
    username: &str,
    current_uid: Uid,
    current_euid: Uid,
    current_gid: Gid,
    current_egid: Gid,
    target_uid: Uid,
    target_gid: Gid,
) -> Result<ChildCredentialSetup, LaunchWorkerError> {
    if current_euid.is_root() {
        return Ok(ChildCredentialSetup::DropFromRoot);
    }

    if current_uid == target_uid
        && current_euid == target_uid
        && current_gid == target_gid
        && current_egid == target_gid
    {
        return Ok(ChildCredentialSetup::AlreadyTarget);
    }

    RootRequiredSnafu {
        username: username.to_string(),
        uid: target_uid.as_raw(),
        gid: target_gid.as_raw(),
        current_uid: current_uid.as_raw(),
        current_gid: current_gid.as_raw(),
    }
    .fail()
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ChildPrivilegeError {
    #[snafu(display("failed to set worker primary group to {gid}"))]
    SetGid { gid: u32, source: nix::Error },

    #[snafu(display("failed to initialize worker supplementary groups for uid {uid}"))]
    InitGroups {
        uid: u32,
        gid: u32,
        source: nix::Error,
    },

    #[snafu(display("failed to set worker uid to {uid}"))]
    SetUid { uid: u32, source: nix::Error },
}

fn initgroups(username: &CStr, gid: Gid) -> nix::Result<()> {
    let gid: libc::gid_t = gid.into();
    #[cfg(target_vendor = "apple")]
    let result = unsafe { libc::initgroups(username.as_ptr(), gid as libc::c_int) };
    #[cfg(not(target_vendor = "apple"))]
    let result = unsafe { libc::initgroups(username.as_ptr(), gid) };
    Errno::result(result).map(drop)
}

fn drop_worker_privileges(
    username: &CStr,
    uid: Uid,
    gid: Gid,
) -> Result<(), ChildPrivilegeError> {
    use child_privilege_error::*;

    setgid(gid).context(SetGidSnafu { gid: gid.as_raw() })?;
    initgroups(username, gid).context(InitGroupsSnafu {
        uid: uid.as_raw(),
        gid: gid.as_raw(),
    })?;
    setuid(uid).context(SetUidSnafu { uid: uid.as_raw() })?;
    Ok(())
}

pub fn launch_worker(
    worker_bin: &Path,
    uid: Uid,
    gid: Gid,
    username: &str,
    home: &Path,
) -> Result<LaunchedWorker, LaunchWorkerError> {
    let worker_bin =
        CString::new(worker_bin.as_os_str().as_encoded_bytes()).context(InvalidWorkerPathSnafu)?;
    let username_cstr = CString::new(username).context(InvalidUsernameSnafu {
        username: username.to_string(),
    })?;
    let credential_setup =
        plan_child_credentials(username, getuid(), geteuid(), getgid(), getegid(), uid, gid)?;

    let env = build_exec_env(username, home).context(BuildExecEnvSnafu { username })?;
    let argv = vec![worker_bin.clone()];
    let max_fd = max_fd();

    // Single SOCK_STREAM socketpair for MuxChannel (replaces stdin/stdout pipes + seqpacket).
    let (parent_fd, child_fd) = std_unix_socketpair().context(CreateSocketpairSnafu)?;

    // SAFETY: fork semantics require unsafe; child path immediately performs exec/exit only.
    match unsafe { fork() }.context(ForkWorkerSnafu)? {
        ForkResult::Child => {
            child_exec(ChildExecSpec {
                worker_bin: &worker_bin,
                argv: &argv,
                envp: &env,
                uid,
                gid,
                username: &username_cstr,
                credential_setup,
                mux_fd: &child_fd,
                max_fd,
            });
        }
        ForkResult::Parent { child } => {
            drop(child_fd);

            Ok(LaunchedWorker {
                handle: WorkerHandle::from_pid(child),
                mux_fd: parent_fd,
            })
        }
    }
}

fn child_exec(spec: ChildExecSpec<'_>) -> ! {
    let ChildExecSpec {
        worker_bin,
        argv,
        envp,
        uid,
        gid,
        username,
        credential_setup,
        mux_fd,
        max_fd,
    } = spec;

    // dup2 the MuxChannel socketpair FD to FD 3 and clear CLOEXEC so the
    // exec'd worker can recover it as its bootstrap channel.
    if install_child_ipc_fd(mux_fd).is_err() {
        child_fail(126);
    }

    // Close all FDs from 4 onward (FD 0/1/2 = inherited stdio for logging,
    // FD 3 = MuxChannel).
    let mut fd = 4;
    while fd < max_fd {
        let _ = nix::unistd::close(fd);
        fd += 1;
    }

    let current_uid = getuid();
    let current_euid = geteuid();
    let current_gid = getgid();
    let current_egid = getegid();

    match credential_setup {
        ChildCredentialSetup::DropFromRoot => {
            if drop_worker_privileges(username, uid, gid).is_err() {
                child_fail(126);
            }
        }
        ChildCredentialSetup::AlreadyTarget => {
            if current_uid != uid
                || current_euid != uid
                || current_gid != gid
                || current_egid != gid
            {
                child_fail(126);
            }
        }
    }

    if getuid() != uid || geteuid() != uid || getgid() != gid || getegid() != gid {
        child_fail(126);
    }

    let _ = execve(worker_bin, argv, envp);
    child_fail(127);
}

/// Create a `SOCK_STREAM` socketpair for MuxChannel IPC.
///
/// Uses `std::os::unix::net::UnixStream::pair()` which sets CLOEXEC
/// automatically. The child side will be dup2'd to a fixed FD, clearing
/// CLOEXEC for that copy.
fn std_unix_socketpair() -> Result<(OwnedFd, OwnedFd), std::io::Error> {
    let (a, b) = std::os::unix::net::UnixStream::pair()?;
    Ok((a.into(), b.into()))
}

/// Spawn a session child process **without dropping privileges**.
///
/// The child process is exec'd as root so it can perform PAM authentication
/// and `open_session`. Returns the socketpair FD for MuxChannel communication.
#[cfg(feature = "sshd")]
pub fn launch_session(username: &str) -> Result<SessionTransport, LaunchSessionError> {
    let session_bin = session_binary_path();
    let session_bin = CString::new(session_bin.as_os_str().as_encoded_bytes())
        .context(InvalidSessionPathSnafu)?;

    let env = build_session_exec_env(username).context(BuildSessionExecEnvSnafu { username })?;
    let argv = vec![session_bin.clone()];
    let max_fd = max_fd();

    // Single SOCK_STREAM socketpair for MuxChannel to session child.
    let (parent_fd, child_fd) = std_unix_socketpair().context(CreateSessionSocketpairSnafu)?;

    // SAFETY: fork semantics require unsafe; child path immediately performs exec/exit only.
    match unsafe { fork() }.context(ForkSessionSnafu)? {
        ForkResult::Child => {
            // Session child: dup2 socketpair to FD 3, close other FDs, exec.
            // No privilege drop — stays root for PAM.
            session_child_exec(&session_bin, &argv, &env, &child_fd, max_fd);
        }
        ForkResult::Parent { child } => {
            drop(child_fd);
            Ok(SessionTransport {
                mux_fd: parent_fd,
                child_pid: child,
            })
        }
    }
}

/// Minimal child exec for session processes: dup2 MuxChannel to FD 3, close FDs, exec.
/// No privilege drop (child runs as root for PAM).
#[cfg(feature = "sshd")]
fn session_child_exec(
    bin: &CString,
    argv: &[CString],
    envp: &[CString],
    mux_fd: &OwnedFd,
    max_fd: i32,
) -> ! {
    // dup2 the MuxChannel socketpair FD to FD 3 and clear CLOEXEC so the
    // exec'd session child can recover it as its bootstrap channel.
    if install_child_ipc_fd(mux_fd).is_err() {
        child_fail(126);
    }

    let mut fd = 4;
    while fd < max_fd {
        let _ = nix::unistd::close(fd);
        fd += 1;
    }

    let _ = execve(bin, argv, envp);
    child_fail(127);
}

#[cfg(feature = "sshd")]
fn build_session_exec_env(username: &str) -> Result<Vec<CString>, BuildExecEnvError> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    [
        [b"PISHOO_USER=".as_slice(), username.as_bytes()].concat(),
        [b"PATH=".as_slice(), path.as_os_str().as_encoded_bytes()].concat(),
    ]
    .into_iter()
    .map(|entry| CString::new(entry).context(EntryContainsNulSnafu))
    .collect()
}

fn build_exec_env(username: &str, home: &Path) -> Result<Vec<CString>, BuildExecEnvError> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let mut entries: Vec<Vec<u8>> = vec![
        [b"HOME=".as_slice(), home.as_os_str().as_encoded_bytes()].concat(),
        [b"USER=".as_slice(), username.as_bytes()].concat(),
        [b"LOGNAME=".as_slice(), username.as_bytes()].concat(),
        [b"PATH=".as_slice(), path.as_os_str().as_encoded_bytes()].concat(),
        [b"PISHOO_USER=".as_slice(), username.as_bytes()].concat(),
    ];
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        entries.push([b"RUST_LOG=".as_slice(), rust_log.as_bytes()].concat());
    }
    entries
        .into_iter()
        .map(|entry| CString::new(entry).context(EntryContainsNulSnafu))
        .collect()
}

fn child_fail(code: i32) -> ! {
    unsafe { libc::_exit(code) }
}

/// Resolve the path of the `pishoo-ssh-session` binary.
///
/// Search order:
/// 1. Runtime env var `PISHOO_SSH_SESSION_BIN`
/// 2. Compile-time env var `PISHOO_SSH_SESSION_BIN` (set by deb builds)
/// 3. `<exe_dir>/../libexec/pishoo-ssh-session` (Homebrew layout)
/// 4. `<exe_dir>/pishoo-ssh-session` (debug / same-dir fallback)
#[cfg(feature = "sshd")]
pub fn session_binary_path() -> std::path::PathBuf {
    // 1. Runtime environment variable
    if let Ok(path) = std::env::var("PISHOO_SSH_SESSION_BIN") {
        return std::path::PathBuf::from(path);
    }

    // 2. Compile-time environment variable (set during release deb builds)
    if let Some(path) = option_env!("PISHOO_SSH_SESSION_BIN") {
        return std::path::PathBuf::from(path);
    }

    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        // 3. Homebrew libexec layout
        let libexec = exe_dir.join("../libexec/pishoo-ssh-session");
        if libexec.exists() {
            return libexec;
        }

        // 4. Same directory fallback
        return exe_dir.join("pishoo-ssh-session");
    }

    std::path::PathBuf::from("pishoo-ssh-session")
}

fn max_fd() -> i32 {
    match sysconf(SysconfVar::OPEN_MAX) {
        Ok(Some(open_max)) if open_max > 0 => open_max as i32,
        _ => 1024,
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    use super::*;

    #[test]
    fn credential_setup_uses_root_drop_when_effective_root() {
        let setup = plan_child_credentials(
            "alice",
            Uid::from_raw(0),
            Uid::from_raw(0),
            Gid::from_raw(0),
            Gid::from_raw(0),
            Uid::from_raw(501),
            Gid::from_raw(20),
        )
        .expect("root can launch target worker");

        assert_eq!(setup, ChildCredentialSetup::DropFromRoot);
    }

    #[test]
    fn credential_setup_accepts_already_target_identity() {
        let setup = plan_child_credentials(
            "alice",
            Uid::from_raw(501),
            Uid::from_raw(501),
            Gid::from_raw(20),
            Gid::from_raw(20),
            Uid::from_raw(501),
            Gid::from_raw(20),
        )
        .expect("already-target process can exec worker");

        assert_eq!(setup, ChildCredentialSetup::AlreadyTarget);
    }

    #[test]
    fn credential_setup_rejects_non_root_mismatched_identity() {
        let error = plan_child_credentials(
            "alice",
            Uid::from_raw(501),
            Uid::from_raw(501),
            Gid::from_raw(20),
            Gid::from_raw(20),
            Uid::from_raw(502),
            Gid::from_raw(20),
        )
        .expect_err("non-root process cannot launch a different worker uid");

        assert!(matches!(
            error,
            LaunchWorkerError::RootRequired {
                uid: 502,
                gid: 20,
                current_uid: 501,
                current_gid: 20,
                ..
            }
        ));
    }

    #[test]
    fn install_child_ipc_fd_clears_cloexec_when_source_is_fd3() {
        const PROBE_ENV: &str = "PISHOO_LAUNCHER_FD3_PROBE";

        if std::env::var_os(PROBE_ENV).is_some() {
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().expect("current test exe"))
            .env(PROBE_ENV, "1")
            .arg("--exact")
            .arg("hypervisor::launcher::tests::install_child_ipc_fd_source_fd3_probe")
            .arg("--nocapture")
            .output()
            .expect("spawn fd3 probe test");

        assert!(
            output.status.success(),
            "fd3 probe failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn install_child_ipc_fd_source_fd3_probe() {
        if std::env::var_os("PISHOO_LAUNCHER_FD3_PROBE").is_none() {
            return;
        }

        let (source, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let raw_source = source.into_raw_fd();

        let fd3 = unsafe {
            if raw_source != CHILD_IPC_FD {
                assert!(
                    libc::dup2(raw_source, CHILD_IPC_FD) >= 0,
                    "dup source fd to fd3 failed: {}",
                    std::io::Error::last_os_error()
                );
                assert!(
                    libc::close(raw_source) == 0,
                    "close source fd failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            OwnedFd::from_raw_fd(CHILD_IPC_FD)
        };

        let flags = unsafe { libc::fcntl(CHILD_IPC_FD, libc::F_GETFD) };
        assert!(
            flags >= 0,
            "read fd3 flags failed: {}",
            std::io::Error::last_os_error()
        );
        assert!(
            unsafe { libc::fcntl(CHILD_IPC_FD, libc::F_SETFD, flags | libc::FD_CLOEXEC) } >= 0,
            "set fd3 cloexec failed: {}",
            std::io::Error::last_os_error()
        );

        install_child_ipc_fd(&fd3).expect("install child ipc fd");

        let flags = unsafe { libc::fcntl(CHILD_IPC_FD, libc::F_GETFD) };
        assert!(
            flags >= 0,
            "read fd3 flags after install failed: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
    }
}

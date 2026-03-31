use std::{
    ffi::{CStr, CString},
    os::fd::OwnedFd,
    path::Path,
};

use nix::{
    errno::Errno,
    unistd::{
        ForkResult, Gid, SysconfVar, Uid, execve, fork, getegid, geteuid, getgid, getuid, pipe,
        setgid, setuid, sysconf,
    },
};
use snafu::{ResultExt, Snafu};
use tokio::fs::File;

use crate::root::worker_handle::WorkerHandle;

pub struct WorkerTransport {
    pub stdin: File,
    pub stdout: File,
}

pub struct LaunchedWorker {
    pub handle: WorkerHandle,
    pub transport: WorkerTransport,
    /// Root-side end of the seqpacket pair for FD passing.
    #[cfg(feature = "sshd")]
    pub seqpacket: OwnedFd,
}

/// Transport for a launched session child process (stdin/stdout pipes).
#[cfg(feature = "sshd")]
pub struct SessionTransport {
    pub stdin: OwnedFd,
    pub stdout: OwnedFd,
}

struct ChildExecSpec<'a> {
    worker_bin: &'a CString,
    argv: &'a [CString],
    envp: &'a [CString],
    uid: Uid,
    gid: Gid,
    supplementary_groups: &'a [Gid],
    stdin_fd: &'a OwnedFd,
    stdout_fd: &'a OwnedFd,
    /// Optional extra FD to dup2 to FD 3 (e.g. seqpacket for FD passing).
    extra_fd: Option<&'a OwnedFd>,
    max_fd: i32,
}

#[derive(Debug, Snafu)]
pub enum LaunchWorkerError {
    #[snafu(display("failed to resolve supplementary groups for user `{username}`"))]
    ResolveSupplementaryGroups {
        username: String,
        source: ResolveSupplementaryGroupsError,
    },
    #[snafu(display("worker path contains nul byte"))]
    InvalidWorkerPath { source: std::ffi::NulError },
    #[snafu(display("failed to build exec environment for user `{username}`"))]
    BuildExecEnv {
        username: String,
        source: BuildExecEnvError,
    },
    #[snafu(display("failed to create worker stdin pipe"))]
    CreateStdinPipe { source: Errno },
    #[snafu(display("failed to create worker stdout pipe"))]
    CreateStdoutPipe { source: Errno },
    #[cfg(feature = "sshd")]
    #[snafu(display("failed to create seqpacket pair"))]
    CreateSeqpacketPair { source: Errno },
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
    #[snafu(display("failed to create session stdin pipe"))]
    CreateSessionStdinPipe { source: Errno },
    #[snafu(display("failed to create session stdout pipe"))]
    CreateSessionStdoutPipe { source: Errno },
    #[snafu(display("failed to fork session process"))]
    ForkSession { source: Errno },
}

#[derive(Debug, Snafu)]
pub enum BuildExecEnvError {
    #[snafu(display("exec environment contains nul byte"))]
    EntryContainsNul { source: std::ffi::NulError },
}

#[derive(Debug, Snafu)]
pub enum ResolveSupplementaryGroupsError {
    #[snafu(display("username contains nul byte"))]
    InvalidUsername { source: std::ffi::NulError },
    #[snafu(display("failed to query supplementary groups for user `{username}`"))]
    GetGroupList { username: String, source: Errno },
    #[snafu(display("user `{username}` has too many supplementary groups ({actual} > {limit})"))]
    TooManyGroups {
        username: String,
        actual: usize,
        limit: usize,
    },
}

pub fn launch_worker(
    worker_bin: &Path,
    uid: Uid,
    gid: Gid,
    username: &str,
    home: &Path,
) -> Result<LaunchedWorker, LaunchWorkerError> {
    let supplementary_groups = resolve_supplementary_groups(username, gid)
        .context(ResolveSupplementaryGroupsSnafu { username })?;
    let worker_bin =
        CString::new(worker_bin.as_os_str().as_encoded_bytes()).context(InvalidWorkerPathSnafu)?;

    #[cfg(feature = "sshd")]
    let (parent_seqpacket, child_seqpacket) = seqpacket_pair().context(CreateSeqpacketPairSnafu)?;

    // Build env — include PISHOO_SEQPACKET_FD if sshd is enabled.
    #[cfg(feature = "sshd")]
    let seqpacket_fd_num = {
        use std::os::fd::AsRawFd;
        child_seqpacket.as_raw_fd()
    };
    let env = build_exec_env(
        username,
        home,
        #[cfg(feature = "sshd")]
        Some(seqpacket_fd_num),
        #[cfg(not(feature = "sshd"))]
        None,
    )
    .context(BuildExecEnvSnafu { username })?;
    let argv = vec![worker_bin.clone()];
    let max_fd = max_fd();

    let (child_stdin_read, parent_stdin_write) = pipe_pair().context(CreateStdinPipeSnafu)?;
    let (parent_stdout_read, child_stdout_write) = pipe_pair().context(CreateStdoutPipeSnafu)?;

    // SAFETY: fork semantics require unsafe; child path immediately performs exec/exit only.
    match unsafe { fork() }.context(ForkWorkerSnafu)? {
        ForkResult::Child => {
            child_exec(ChildExecSpec {
                worker_bin: &worker_bin,
                argv: &argv,
                envp: &env,
                uid,
                gid,
                supplementary_groups: &supplementary_groups,
                stdin_fd: &child_stdin_read,
                stdout_fd: &child_stdout_write,
                #[cfg(feature = "sshd")]
                extra_fd: Some(&child_seqpacket),
                #[cfg(not(feature = "sshd"))]
                extra_fd: None,
                max_fd,
            });
        }
        ForkResult::Parent { child } => {
            drop(child_stdin_read);
            drop(child_stdout_write);
            #[cfg(feature = "sshd")]
            drop(child_seqpacket);
            let stdin = File::from_std(std::fs::File::from(parent_stdin_write));
            let stdout = File::from_std(std::fs::File::from(parent_stdout_read));

            Ok(LaunchedWorker {
                handle: WorkerHandle::from_pid(child),
                transport: WorkerTransport { stdin, stdout },
                #[cfg(feature = "sshd")]
                seqpacket: parent_seqpacket,
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
        supplementary_groups,
        stdin_fd,
        stdout_fd,
        extra_fd,
        max_fd,
    } = spec;

    if nix::unistd::dup2_stdin(stdin_fd).is_err() {
        child_fail(126);
    }
    if nix::unistd::dup2_stdout(stdout_fd).is_err() {
        child_fail(126);
    }

    // Determine the FD to skip when closing (the extra FD we want to pass).
    let skip_fd = extra_fd.map(|fd| {
        use std::os::fd::AsRawFd;
        fd.as_raw_fd()
    });

    let start_fd = 3;
    let mut fd = start_fd;
    while fd < max_fd {
        if Some(fd) != skip_fd {
            let _ = nix::unistd::close(fd);
        }
        fd += 1;
    }

    let current_uid = getuid();
    let current_euid = geteuid();
    let current_gid = getgid();
    let current_egid = getegid();

    if current_euid.is_root() {
        if setgroups(supplementary_groups).is_err() {
            child_fail(126);
        }
        if setgid(gid).is_err() {
            child_fail(126);
        }
        if setuid(uid).is_err() {
            child_fail(126);
        }
        if getuid() != uid || geteuid() != uid || getgid() != gid || getegid() != gid {
            child_fail(126);
        }
    } else if current_uid != uid || current_euid != uid || current_gid != gid || current_egid != gid
    {
        child_fail(126);
    }

    let _ = execve(worker_bin, argv, envp);
    child_fail(127);
}

fn pipe_pair() -> Result<(OwnedFd, OwnedFd), Errno> {
    pipe()
}

#[cfg(feature = "sshd")]
fn seqpacket_pair() -> Result<(OwnedFd, OwnedFd), Errno> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    let (a, b) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::SOCK_CLOEXEC,
    )?;
    Ok((a, b))
}

/// Spawn a session child process **without dropping privileges**.
///
/// The child process is exec'd as root so it can perform PAM authentication
/// and `open_session`. It is responsible for calling `drop_privileges()`
/// after PAM completes.
///
/// Returns the pipe file descriptors for communicating with the child
/// via remoc (stdin write + stdout read).
#[cfg(feature = "sshd")]
pub fn launch_session(username: &str) -> Result<SessionTransport, LaunchSessionError> {
    let session_bin = session_binary_path();
    let session_bin = CString::new(session_bin.as_os_str().as_encoded_bytes())
        .context(InvalidSessionPathSnafu)?;

    let env = build_session_exec_env(username).context(BuildSessionExecEnvSnafu { username })?;
    let argv = vec![session_bin.clone()];
    let max_fd = max_fd();

    let (child_stdin_read, parent_stdin_write) =
        pipe_pair().context(CreateSessionStdinPipeSnafu)?;
    let (parent_stdout_read, child_stdout_write) =
        pipe_pair().context(CreateSessionStdoutPipeSnafu)?;

    // SAFETY: fork semantics require unsafe; child path immediately performs exec/exit only.
    match unsafe { fork() }.context(ForkSessionSnafu)? {
        ForkResult::Child => {
            // Session child: dup2 stdin/stdout, close other FDs, exec.
            // No privilege drop — stays root for PAM.
            session_child_exec(
                &session_bin,
                &argv,
                &env,
                &child_stdin_read,
                &child_stdout_write,
                max_fd,
            );
        }
        ForkResult::Parent { child: _ } => {
            drop(child_stdin_read);
            drop(child_stdout_write);
            Ok(SessionTransport {
                stdin: parent_stdin_write,
                stdout: parent_stdout_read,
            })
        }
    }
}

/// Minimal child exec for session processes: dup2 stdin/stdout, close FDs, exec.
/// No privilege drop (child runs as root for PAM).
#[cfg(feature = "sshd")]
fn session_child_exec(
    bin: &CString,
    argv: &[CString],
    envp: &[CString],
    stdin_fd: &OwnedFd,
    stdout_fd: &OwnedFd,
    max_fd: i32,
) -> ! {
    if nix::unistd::dup2_stdin(stdin_fd).is_err() {
        child_fail(126);
    }
    if nix::unistd::dup2_stdout(stdout_fd).is_err() {
        child_fail(126);
    }

    let mut fd = 3;
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

fn build_exec_env(
    username: &str,
    home: &Path,
    seqpacket_fd: Option<i32>,
) -> Result<Vec<CString>, BuildExecEnvError> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let mut entries: Vec<Vec<u8>> = vec![
        [b"HOME=".as_slice(), home.as_os_str().as_encoded_bytes()].concat(),
        [b"USER=".as_slice(), username.as_bytes()].concat(),
        [b"LOGNAME=".as_slice(), username.as_bytes()].concat(),
        [b"PATH=".as_slice(), path.as_os_str().as_encoded_bytes()].concat(),
        [b"PISHOO_USER=".as_slice(), username.as_bytes()].concat(),
    ];
    if let Some(fd) = seqpacket_fd {
        entries.push(format!("PISHOO_SEQPACKET_FD={fd}").into_bytes());
    }
    entries
        .into_iter()
        .map(|entry| CString::new(entry).context(EntryContainsNulSnafu))
        .collect()
}

fn child_fail(code: i32) -> ! {
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// SCM_RIGHTS helpers for FD passing over seqpacket
// ---------------------------------------------------------------------------

/// Send file descriptors over a seqpacket socket using `SCM_RIGHTS`.
///
/// Blocking — should be called inside `spawn_blocking`.
#[cfg(feature = "sshd")]
pub fn send_fds(sock_fd: std::os::fd::RawFd, fds: &[std::os::fd::RawFd]) -> Result<(), Errno> {
    use std::io::IoSlice;

    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};

    let cmsg = [ControlMessage::ScmRights(fds)];
    sendmsg::<()>(
        sock_fd,
        &[IoSlice::new(&[0u8])],
        &cmsg,
        MsgFlags::empty(),
        None,
    )?;
    Ok(())
}

/// Receive file descriptors from a seqpacket socket using `SCM_RIGHTS`.
///
/// Blocking — should be called inside `spawn_blocking`.
#[cfg(feature = "sshd")]
pub fn recv_fds(sock_fd: std::os::fd::RawFd) -> Result<Vec<OwnedFd>, Errno> {
    use std::{io::IoSliceMut, os::fd::FromRawFd};

    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};

    let mut buf = [0u8; 1];
    let mut cmsg_buf = nix::cmsg_space!([std::os::fd::RawFd; 4]);
    let mut iov = [IoSliceMut::new(&mut buf)];
    let msg = recvmsg::<()>(sock_fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty())?;
    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            // SAFETY: recvmsg guarantees these are valid open FDs passed via SCM_RIGHTS.
            return Ok(fds
                .into_iter()
                .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
                .collect());
        }
    }
    Err(Errno::ENODATA)
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

fn resolve_supplementary_groups(
    username: &str,
    gid: Gid,
) -> Result<Vec<Gid>, ResolveSupplementaryGroupsError> {
    let username_cstr = CString::new(username).context(InvalidUsernameSnafu)?;
    let groups = getgrouplist(&username_cstr, gid).context(GetGroupListSnafu {
        username: username.to_string(),
    })?;
    let groups = normalize_supplementary_groups(groups, gid);
    let limit = supplementary_groups_max();
    if groups.len() > limit {
        return TooManyGroupsSnafu {
            username: username.to_string(),
            actual: groups.len(),
            limit,
        }
        .fail();
    }

    Ok(groups)
}

fn normalize_supplementary_groups(groups: Vec<Gid>, primary_gid: Gid) -> Vec<Gid> {
    let mut normalized = Vec::with_capacity(groups.len());
    for group in groups {
        if group == primary_gid || normalized.contains(&group) {
            continue;
        }
        normalized.push(group);
    }
    normalized
}

fn supplementary_groups_max() -> usize {
    match sysconf(SysconfVar::NGROUPS_MAX) {
        Ok(Some(limit)) if limit > 0 => limit as usize,
        _ => 16,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn getgrouplist(username: &CStr, group: Gid) -> Result<Vec<Gid>, Errno> {
    nix::unistd::getgrouplist(username, group)
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn setgroups(groups: &[Gid]) -> Result<(), Errno> {
    nix::unistd::setgroups(groups)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn getgrouplist(username: &CStr, group: Gid) -> Result<Vec<Gid>, Errno> {
    let basegid = libc::c_int::try_from(group.as_raw()).map_err(|_| Errno::EINVAL)?;
    let mut ngroups: libc::c_int = 16;
    let mut groups: Vec<libc::c_int> = vec![0; ngroups as usize];

    loop {
        let mut ngroups_attempt = ngroups;
        let rc = unsafe {
            libc::getgrouplist(
                username.as_ptr(),
                basegid,
                groups.as_mut_ptr(),
                &mut ngroups_attempt,
            )
        };

        if rc >= 0 {
            groups.truncate(ngroups_attempt as usize);
            return Ok(groups
                .into_iter()
                .map(|gid| Gid::from_raw(gid as libc::gid_t))
                .collect());
        }

        if ngroups_attempt <= 0 {
            return Err(Errno::last());
        }

        if ngroups_attempt > ngroups {
            ngroups = ngroups_attempt;
        } else {
            ngroups = ngroups.saturating_mul(2).max(16);
        }
        groups.resize(ngroups as usize, 0);
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn setgroups(groups: &[Gid]) -> Result<(), Errno> {
    let raw_groups: Vec<libc::gid_t> = groups.iter().map(|gid| gid.as_raw()).collect();
    let (ptr, len) = if raw_groups.is_empty() {
        (std::ptr::null(), 0)
    } else {
        (raw_groups.as_ptr(), raw_groups.len() as libc::c_int)
    };

    let rc = unsafe { libc::setgroups(len, ptr) };
    if rc == 0 { Ok(()) } else { Err(Errno::last()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_supplementary_groups_drops_primary_and_duplicates() {
        let primary_gid = Gid::from_raw(20);
        let normalized = normalize_supplementary_groups(
            vec![
                primary_gid,
                Gid::from_raw(501),
                Gid::from_raw(12),
                Gid::from_raw(501),
                Gid::from_raw(79),
                primary_gid,
            ],
            primary_gid,
        );

        assert_eq!(
            normalized,
            vec![Gid::from_raw(501), Gid::from_raw(12), Gid::from_raw(79)]
        );
    }
}

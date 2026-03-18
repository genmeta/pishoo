use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};

use nix::unistd::Uid;
use tokio::fs::File;

use crate::worker_spawn::WorkerHandle;

pub struct WorkerTransport {
    pub stdin: File,
    pub stdout: File,
}

pub struct LaunchedWorker {
    pub handle: WorkerHandle,
    pub transport: WorkerTransport,
}

pub fn launch_worker(
    worker_bin: &Path,
    uid: Uid,
    gid: u32,
    username: &str,
    home: &Path,
) -> Result<LaunchedWorker, std::io::Error> {
    let supplementary_groups = resolve_supplementary_groups(username, gid)?;
    let worker_bin = CString::new(worker_bin.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "worker path contains NUL byte",
        )
    })?;
    let env = build_exec_env(username, home)?;
    let argv = [worker_bin.as_ptr(), std::ptr::null()];
    let envp = build_envp(&env);
    let max_fd = max_fd();

    let (child_stdin_read, parent_stdin_write) = pipe_pair()?;
    let (parent_stdout_read, child_stdout_write) = pipe_pair()?;

    let pid = unsafe { libc::fork() };
    if pid == -1 {
        return Err(std::io::Error::last_os_error());
    }

    if pid == 0 {
        child_exec(
            &worker_bin,
            &argv,
            &envp,
            uid,
            gid,
            &supplementary_groups,
            child_stdin_read.as_raw_fd(),
            child_stdout_write.as_raw_fd(),
            max_fd,
        );
    }

    drop(child_stdin_read);
    drop(child_stdout_write);
    let stdin = File::from_std(std::fs::File::from(parent_stdin_write));
    let stdout = File::from_std(std::fs::File::from(parent_stdout_read));

    Ok(LaunchedWorker {
        handle: WorkerHandle::from_unix_pid(pid as u32),
        transport: WorkerTransport { stdin, stdout },
    })
}

fn child_exec(
    worker_bin: &CString,
    argv: &[*const libc::c_char; 2],
    envp: &[*const libc::c_char],
    uid: Uid,
    gid: u32,
    supplementary_groups: &[libc::gid_t],
    stdin_fd: libc::c_int,
    stdout_fd: libc::c_int,
    max_fd: libc::c_int,
) -> ! {
    if unsafe { libc::dup2(stdin_fd, libc::STDIN_FILENO) } == -1 {
        unsafe { libc::_exit(126) };
    }
    if unsafe { libc::dup2(stdout_fd, libc::STDOUT_FILENO) } == -1 {
        unsafe { libc::_exit(126) };
    }

    let mut fd = 3;
    while fd < max_fd {
        unsafe { libc::close(fd) };
        fd += 1;
    }

    if unsafe { libc::setgroups(supplementary_groups.len(), supplementary_groups.as_ptr()) } != 0 {
        unsafe { libc::_exit(126) };
    }
    if unsafe { libc::setgid(gid as libc::gid_t) } != 0 {
        unsafe { libc::_exit(126) };
    }
    if unsafe { libc::setuid(uid.as_raw()) } != 0 {
        unsafe { libc::_exit(126) };
    }
    if unsafe { libc::getuid() } != uid.as_raw()
        || unsafe { libc::geteuid() } != uid.as_raw()
        || unsafe { libc::getgid() } != gid as libc::gid_t
        || unsafe { libc::getegid() } != gid as libc::gid_t
    {
        unsafe { libc::_exit(126) };
    }

    unsafe { libc::execve(worker_bin.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    unsafe { libc::_exit(127) };
}

fn pipe_pair() -> Result<(OwnedFd, OwnedFd), std::io::Error> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read_end, write_end))
}

fn build_exec_env(username: &str, home: &Path) -> Result<Vec<CString>, std::io::Error> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    [
        [b"HOME=".as_slice(), home.as_os_str().as_bytes()].concat(),
        [b"USER=".as_slice(), username.as_bytes()].concat(),
        [b"LOGNAME=".as_slice(), username.as_bytes()].concat(),
        [b"PATH=".as_slice(), path.as_os_str().as_bytes()].concat(),
    ]
    .into_iter()
    .map(|entry| {
        CString::new(entry).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "environment contains NUL byte",
            )
        })
    })
    .collect()
}

fn build_envp(env: &[CString]) -> Vec<*const libc::c_char> {
    let mut envp = env.iter().map(|entry| entry.as_ptr()).collect::<Vec<_>>();
    envp.push(std::ptr::null());
    envp
}

fn max_fd() -> libc::c_int {
    let open_max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if open_max > 0 {
        open_max as libc::c_int
    } else {
        1024
    }
}

fn resolve_supplementary_groups(
    username: &str,
    gid: u32,
) -> Result<Vec<libc::gid_t>, std::io::Error> {
    let username = CString::new(username).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "username contains NUL byte",
        )
    })?;
    let mut ngroups: libc::c_int = 0;
    let _ = unsafe {
        libc::getgrouplist(
            username.as_ptr(),
            gid as libc::gid_t,
            std::ptr::null_mut(),
            &mut ngroups,
        )
    };
    if ngroups <= 0 {
        return Ok(vec![gid as libc::gid_t]);
    }
    let mut groups = vec![0 as libc::gid_t; ngroups as usize];
    let ret = unsafe {
        libc::getgrouplist(
            username.as_ptr(),
            gid as libc::gid_t,
            groups.as_mut_ptr(),
            &mut ngroups,
        )
    };
    if ret == -1 {
        return Err(std::io::Error::other(
            "failed to resolve supplementary groups",
        ));
    }
    groups.truncate(ngroups as usize);
    Ok(groups)
}

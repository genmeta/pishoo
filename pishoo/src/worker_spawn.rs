//! Root-side worker process spawning.
//!
//! Spawns the `pishoo-worker` binary as a target user, establishes a remoc
//! connection over stdin/stdout pipes, and sends [`WorkerBootstrap`] to the child.
//!
//! # Protocol
//!
//! 1. Root spawns `pishoo-worker` with stdin/stdout piped, stderr inherited.
//! 2. `pre_exec` drops privileges: `setgid` → `initgroups` → `setuid`.
//! 3. Root establishes remoc connection: reads from child's stdout, writes to child's stdin.
//! 4. Root sends [`WorkerBootstrap`] (including an mpsc signal channel) via base channel.
//! 5. Child receives bootstrap and sends `()` ack back.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::unistd::{Gid, Uid as NixUid};
use remoc::rtc::ServerShared;
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::protocol::{RootToWorker, Uid, WorkerBootstrap, WorkerHello, RootTransportApiServerShared};

/// Handle to a spawned worker process.
///
/// Holds the child process and a signal channel for sending [`RootToWorker`]
/// messages to the worker. Killing the child on drop ensures cleanup.
pub struct WorkerHandle {
    pub child: Child,
    /// Root → worker signal channel (mpsc sender kept by root).
    pub signal_tx: remoc::rch::mpsc::Sender<RootToWorker>,
}

pub struct SpawnedWorker {
    pub handle: WorkerHandle,
    pub hello: WorkerHello,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Best-effort kill — child may have already exited.
        let _ = self.child.start_kill();
    }
}

/// Spawn a `pishoo-worker` process for the given user and establish remoc channels.
///
/// # Arguments
///
/// * `worker_bin` — Path to the `pishoo-worker` binary.
/// * `uid` — Target user's UID (for privilege drop).
/// * `gid` — Target user's primary GID (for privilege drop).
/// * `username` — Target username (for `initgroups` and bootstrap).
/// * `home` — Target user's home directory.
/// * `log_dir` — Directory where the worker should write logs.
/// * `state` — Shared root state for creating per-worker transport API.
/// # Errors
///
/// Returns an error if the process cannot be spawned or the remoc handshake fails.
pub async fn spawn_worker(
    worker_bin: impl AsRef<Path>,
    uid: Uid,
    gid: u32,
    username: String,
    home: PathBuf,
    log_dir: PathBuf,
    state: Arc<Mutex<crate::root_state::RootState>>,
) -> Result<SpawnedWorker, std::io::Error> {
    let supplementary_groups = resolve_supplementary_groups(&username, gid)?;

    let mut command = tokio::process::Command::new(worker_bin.as_ref());
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    // Pre-exec: drop privileges to target user before exec.
    // Safety: setgid/initgroups/setuid are async-signal-safe on Linux.
    unsafe {
        let groups = supplementary_groups.clone();
        let nix_gid = Gid::from_raw(gid);
        let nix_uid = NixUid::from_raw(uid);
        command.pre_exec(move || {
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            nix::unistd::setgid(nix_gid)?;
            nix::unistd::setuid(nix_uid)?;
            if libc::getuid() != nix_uid.as_raw()
                || libc::geteuid() != nix_uid.as_raw()
                || libc::getgid() != nix_gid.as_raw()
                || libc::getegid() != nix_gid.as_raw()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "credential verification failed after setuid/setgid",
                ));
            }
            Ok(())
        });
    }

    let mut child = command.spawn()?;

    // Take ownership of the piped handles.
    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture child stdin"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture child stdout"))?;

    // Establish remoc connection over the child's stdio.
    // reader = child_stdout (parent reads from child)
    // writer = child_stdin  (parent writes to child)
    //
    // Base channel types (parent perspective):
    //   Sender<WorkerBootstrap>  — parent sends bootstrap to child
    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerBootstrap>,
        remoc::rch::base::Receiver<WorkerHello>,
    ) = remoc::Connect::io(remoc::Cfg::default(), child_stdout, child_stdin)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;
    tokio::spawn(conn);
    // Create the root→worker signal channel (mpsc). The receiver end is embedded
    // in the bootstrap so it crosses the remoc boundary to the child.
    let (signal_tx, signal_rx) = remoc::rch::mpsc::channel(16);

    // Create per-worker RPC API server+client pair.
    let pid = child.id().expect("child has pid");
    let api_impl = crate::root_transport_api::RootTransportApiImpl::new(pid, state.clone());
    let (server, client) = RootTransportApiServerShared::new(Arc::new(api_impl), 1);
    tokio::spawn(async move { server.serve(true).await });

    let bootstrap = WorkerBootstrap {
        uid,
        username,
        home,
        log_dir,
        signal_rx,
        root_api: client,
    };

    base_tx
        .send(bootstrap)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string()))?;

    let hello = base_rx
        .recv()
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string()))?
        .ok_or_else(|| std::io::Error::other("worker closed channel without sending startup hello"))?;

    Ok(SpawnedWorker {
        handle: WorkerHandle { child, signal_tx },
        hello,
    })
}

fn resolve_supplementary_groups(username: &str, gid: u32) -> Result<Vec<libc::gid_t>, std::io::Error> {
    let username = CString::new(username)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "username contains NUL byte"))?;
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
        return Err(std::io::Error::other("failed to resolve supplementary groups"));
    }
    groups.truncate(ngroups as usize);
    Ok(groups)
}

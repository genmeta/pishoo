//! Execute a statically declared command with explicitly delegated directory capabilities.

use std::{collections::HashSet, fs::File, os::unix::fs::MetadataExt, sync::Arc, time::Duration};

use tokio::io::{AsyncReadExt, DuplexStream};
use wasmtime::{
    Store, Trap,
    component::{Linker, ResourceTable},
};
use wasmtime_wasi::{
    FsPerms, I32Exit, WasiCtx, WasiCtxBuilder,
    cli::AsyncStdoutStream,
    filesystem::Descriptor,
    p2::{bindings::Command, pipe::MemoryInputPipe},
};
use wasmtime_wasi_http::WasiHttpCtx;

use super::super::{
    Outgoing, StoreData,
    runtime::{execution_limits, repair_guest_ownership},
};
use crate::{Error, Result};

pub(in crate::sandbox) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit/pishoo-execution",
        world: "execution-client",
        with: { "wasi:filesystem/types": wasmtime_wasi::p2::bindings::filesystem::types },
        imports: { default: async | trappable },
    });
}

use bindings::pishoo::execution::runtime::{
    ExecutionError, ExecutionResult, Request, TerminationReason,
};

const OUTPUT_LIMIT: usize = 1024 * 1024;

#[cfg(test)]
#[path = "../../../tests/unit/sandbox/host/execution.rs"]
mod tests;

pub(in crate::sandbox) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

impl bindings::pishoo::execution::runtime::Host for StoreData {
    async fn execute(
        &mut self,
        request: Request,
    ) -> wasmtime::Result<std::result::Result<ExecutionResult, ExecutionError>> {
        Ok(execute_child(self, request).await)
    }
}

async fn execute_child(
    parent: &mut StoreData,
    request: Request,
) -> std::result::Result<ExecutionResult, ExecutionError> {
    if !valid_name(&request.program)
        || request.args.len() > 512
        || request.args.iter().map(String::len).sum::<usize>() > 2 * 1024 * 1024
        || request.args.iter().any(|value| value.contains('\0'))
        || request.env.len() > 128
        || request
            .env
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
            > 32 * 1024
        || request
            .env
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['\0', '=']) || value.contains('\0'))
        || request.stdin.len() > OUTPUT_LIMIT
        || request.directories.len() > 8
        || !(1..=60_000).contains(&request.timeout_ms)
        || !(1..=10_000_000_000).contains(&request.fuel)
    {
        return Err(ExecutionError::InvalidRequest);
    }
    let program = parent
        .commands
        .get(&request.program)
        .cloned()
        .ok_or(ExecutionError::ProgramNotFound)?;
    let engine = program.engine().clone();
    let mut directories = Vec::new();
    let mut paths = HashSet::new();
    for directory in request.directories {
        if directory.path == "/tmp"
            || !directory.path.starts_with('/')
            || directory.path.len() > 128
            || directory.path.contains(['\0', '\\'])
            || directory.path[1..]
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !paths.insert(directory.path.clone())
        {
            return Err(ExecutionError::InvalidRequest);
        }
        let Descriptor::Dir(source) = parent
            .table
            .get(&directory.descriptor)
            .map_err(|_| ExecutionError::InvalidRequest)?
        else {
            return Err(ExecutionError::InvalidRequest);
        };
        // POSIX directory descriptors open read-only. FsPerms bounds the
        // filesystem operations available through the delegated directory.
        let permissions = if directory.read_only || source.perms.write_not_permitted() {
            FsPerms::ReadOnly
        } else {
            FsPerms::ReadWrite
        };
        directories.push((
            source
                .dir
                .try_clone()
                .map_err(|_| ExecutionError::Unavailable)?,
            directory.path,
            permissions,
        ));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(request.timeout_ms);
    let temporary = tempfile::tempdir().map_err(|_| ExecutionError::Unavailable)?;
    let temporary_file = File::open(temporary.path()).map_err(|_| ExecutionError::Unavailable)?;
    if let Some((directory, _, _)) = directories.first() {
        inherit_owner(&temporary_file, directory).map_err(|_| ExecutionError::Unavailable)?;
    }
    directories.push((temporary_file, "/tmp".into(), FsPerms::ReadWrite));
    let (stdout_read, stdout_write) = tokio::io::duplex(16 * 1024);
    let (stderr_read, stderr_write) = tokio::io::duplex(16 * 1024);
    let mut wasi = WasiCtx::builder();
    for (directory, path, permissions) in &directories {
        preopen(&mut wasi, directory, path, *permissions)
            .map_err(|_| ExecutionError::Unavailable)?;
    }
    wasi.args(&request.args)
        .envs(&request.env)
        .stdin(MemoryInputPipe::new(request.stdin))
        .stdout(AsyncStdoutStream::new(16 * 1024, stdout_write))
        .stderr(AsyncStdoutStream::new(16 * 1024, stderr_write));
    let mut wasi = wasi.build();
    let expected = directories
        .iter()
        .map(|(file, path, _)| (file, path.as_str()))
        .collect::<Vec<_>>();
    verify_preopens(&mut wasi, &expected).map_err(|_| ExecutionError::Unavailable)?;
    let local = parent.local.clone();
    let remote = parent.remote.clone();
    let call = async move {
        let _temporary = temporary;
        let _ownership = scopeguard::guard(wasi.filesystem().clone(), |filesystem| {
            if let Err(error) = repair_guest_ownership(filesystem) {
                tracing::error!(%error,"cannot restore command directory ownership");
            }
        });
        let mut store = Store::new(
            &engine,
            StoreData {
                caller_service: None,
                commands: Arc::default(),
                table: ResourceTable::new(),
                wasi,
                http: WasiHttpCtx::new(),
                memory: execution_limits(),
                outgoing: Outgoing::Denied,
                local,
                remote,
            },
        );
        store.limiter(|state| &mut state.memory);
        store.set_fuel(request.fuel)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(1);
        store.epoch_deadline_async_yield_and_update(1);
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        let outcome = tokio::time::timeout_at(deadline, async {
            let command = Command::instantiate_async(&mut store, &program, &linker).await?;
            command.wasi_cli_run().call_run(&mut store).await
        })
        .await;
        let result = match outcome {
            Err(_) => (TerminationReason::Timeout, None),
            Ok(Ok(Ok(()))) => (TerminationReason::Exited, Some(0)),
            Ok(Ok(Err(()))) => (TerminationReason::Exited, Some(1)),
            Ok(Err(error)) => {
                if let Some(exit) = error.downcast_ref::<I32Exit>() {
                    (TerminationReason::Exited, Some(exit.0 as u8))
                } else if error.downcast_ref::<Trap>() == Some(&Trap::OutOfFuel) {
                    (TerminationReason::FuelExhausted, None)
                } else {
                    tracing::warn!(%error,"command trapped");
                    (TerminationReason::Trap, None)
                }
            }
        };
        Ok::<_, wasmtime::Error>(result)
    };
    let capture = capture(stdout_read, stderr_read);
    tokio::pin!(call, capture);
    let (outcome, (stdout, stderr, truncated)) = tokio::select! {
        outcome=&mut call => (outcome,capture.await.map_err(|_|ExecutionError::Unavailable)?),
        captured=&mut capture => {
            let captured=captured.map_err(|_|ExecutionError::Unavailable)?;
            let outcome=if captured.2 { Ok((TerminationReason::OutputLimit,None)) } else { call.await };
            (outcome,captured)
        },
    };
    let (termination_reason, exit_code) = if truncated {
        (TerminationReason::OutputLimit, None)
    } else {
        outcome.map_err(|_| ExecutionError::Unavailable)?
    };
    Ok(ExecutionResult {
        stdout,
        stderr,
        exit_code,
        termination_reason,
        truncated,
    })
}

fn inherit_owner(file: &File, parent: &File) -> Result<()> {
    if rustix::process::geteuid().is_root() {
        let owner = parent.metadata()?;
        let permissions = file.metadata()?.permissions();
        rustix::fs::fchown(
            file,
            Some(rustix::fs::Uid::from_raw(owner.uid())),
            Some(rustix::fs::Gid::from_raw(owner.gid())),
        )
        .map_err(std::io::Error::from)?;
        file.set_permissions(permissions)?;
    }
    Ok(())
}

fn preopen(
    wasi: &mut WasiCtxBuilder,
    file: &File,
    guest: &str,
    permissions: FsPerms,
) -> Result<()> {
    // Builder 49 accepts paths. Each opened grant is checked against the held
    // directory's device/inode before the Store can execute.
    #[cfg(target_os = "linux")]
    let path = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(file));
    #[cfg(target_os = "macos")]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        let path = rustix::fs::getpath(file).map_err(std::io::Error::from)?;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()))
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let path = format!("/dev/fd/{}", std::os::fd::AsRawFd::as_raw_fd(file));
    wasi.preopened_dir(path, guest, permissions)
        .map_err(Error::Guest)?;
    Ok(())
}

fn verify_preopens(wasi: &mut WasiCtx, expected: &[(&File, &str)]) -> Result<()> {
    use wasmtime_wasi::p2::bindings::filesystem::preopens::Host;
    let mut table = ResourceTable::new();
    let directories = wasmtime_wasi::filesystem::WasiFilesystemCtxView {
        ctx: wasi.filesystem(),
        table: &mut table,
    }
    .get_directories()
    .map_err(Error::Guest)?;
    for (file, name) in expected {
        let (descriptor, _) = directories
            .iter()
            .find(|(_, path)| path == name)
            .ok_or_else(|| Error::InvalidConfig("missing command directory grant".into()))?;
        let wasmtime_wasi::filesystem::Descriptor::Dir(directory) = table
            .get(descriptor)
            .map_err(|error| Error::Guest(error.into()))?
        else {
            return Err(Error::InvalidConfig(
                "invalid command directory grant".into(),
            ));
        };
        let actual = directory.dir.metadata()?;
        let expected = file.metadata()?;
        if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
            return Err(Error::InvalidConfig(
                "command directory changed while granting access".into(),
            ));
        }
    }
    Ok(())
}

async fn capture(
    mut stdout: DuplexStream,
    mut stderr: DuplexStream,
) -> std::io::Result<(Vec<u8>, Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut stdout_buffer = [0; 16 * 1024];
    let mut stderr_buffer = [0; 16 * 1024];
    let mut stdout_open = true;
    let mut stderr_open = true;
    while stdout_open || stderr_open {
        let remaining = OUTPUT_LIMIT - output.len() - errors.len();
        let (destination, bytes) = tokio::select! {
            read = stdout.read(&mut stdout_buffer), if stdout_open => {
                let count = read?;
                stdout_open = count != 0;
                (&mut output, &stdout_buffer[..count])
            },
            read = stderr.read(&mut stderr_buffer), if stderr_open => {
                let count = read?;
                stderr_open = count != 0;
                (&mut errors, &stderr_buffer[..count])
            },
        };
        destination.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if output.len() + errors.len() == OUTPUT_LIMIT {
            return Ok((output, errors, true));
        }
    }
    Ok((output, errors, false))
}

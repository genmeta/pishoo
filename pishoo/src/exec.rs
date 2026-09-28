//! One bounded, non-interactive host command per authorized request.

use std::{os::unix::process::ExitStatusExt, path::Path, process::Stdio, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::{Pid, geteuid},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::oneshot,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{Body, Error, Result};

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDIN_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) async fn execute(
    enabled: bool,
    server_name: &str,
    cwd: &Path,
    tasks: TaskTracker,
    request: Request<Body>,
) -> Result<Response<Body>> {
    if !enabled {
        return Ok(empty(StatusCode::NOT_FOUND));
    }
    let summary = request
        .extensions()
        .get::<dhttp::HandshakeSummary>()
        .ok_or(Error::MissingHandshake)?;
    let caller = summary
        .remote
        .as_ref()
        .and_then(|remote| dhttp_home::normalize_name(remote.name()))
        .ok_or(Error::Denied)?;
    if caller != server_name {
        return Err(Error::Denied);
    }
    execute_authorized(cwd, tasks, request).await
}

async fn execute_authorized(
    cwd: &Path,
    tasks: TaskTracker,
    request: Request<Body>,
) -> Result<Response<Body>> {
    if request.method() != Method::POST {
        let mut response = empty(StatusCode::METHOD_NOT_ALLOWED);
        response
            .headers_mut()
            .insert(header::ALLOW, http::HeaderValue::from_static("POST"));
        return Ok(response);
    }
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some("application/json")
    {
        return Err(Error::BadRequest("exec requires application/json".into()));
    }
    if geteuid().is_root() {
        return Err(Error::BackendUnavailable(
            "exec refuses to run under a root service account".into(),
        ));
    }
    if tasks.is_closed() {
        return Err(Error::Closed);
    }

    let mut body = request.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let data = frame
            .map_err(|error| Error::Io(std::io::Error::other(error)))?
            .into_data()
            .map_err(|_| Error::BadRequest("exec request trailers are unsupported".into()))?;
        if data.len() > MAX_REQUEST_BYTES.saturating_sub(bytes.len()) {
            return Err(Error::BadRequest("exec request is too large".into()));
        }
        bytes.extend_from_slice(&data);
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::BadRequest("invalid exec JSON".into()))?;
    let fields = value
        .as_object_mut()
        .ok_or_else(|| Error::BadRequest("exec request must be an object".into()))?;
    let program = fields
        .remove("program")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| Error::BadRequest("exec program is required".into()))?;
    let args: Vec<String> = fields
        .remove("args")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| Error::BadRequest("exec args must be strings".into()))?
        .unwrap_or_default();
    let stdin = fields
        .remove("stdin_base64")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::BadRequest("stdin_base64 must be a string".into()))
                .and_then(|text| {
                    STANDARD
                        .decode(text)
                        .map_err(|_| Error::BadRequest("invalid stdin_base64".into()))
                })
        })
        .transpose()?
        .unwrap_or_default();
    if !fields.is_empty() {
        return Err(Error::BadRequest("unknown exec request field".into()));
    }
    if program.is_empty()
        || program.len() > 4096
        || program.contains('\0')
        || args.len() > 128
        || args.iter().any(|arg| arg.contains('\0'))
        || args.iter().map(String::len).sum::<usize>() > MAX_ARGUMENT_BYTES
        || stdin.len() > MAX_STDIN_BYTES
    {
        return Err(Error::BadRequest("invalid exec arguments or stdin".into()));
    }

    if tasks.is_closed() {
        return Err(Error::Closed);
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .env("HOME", cwd)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::BadRequest("exec program was not found".into())
        } else {
            Error::Io(error)
        }
    })?;
    let cancel = CancellationToken::new();
    let cancel_on_drop = cancel.clone().drop_guard();
    let (tx, rx) = oneshot::channel();
    tasks.spawn(async move {
        let result = run_command(child, stdin, cancel).await;
        let _ = tx.send(result);
    });
    let (status, stdout, stderr) = rx.await.map_err(|_| Error::Closed)??;
    cancel_on_drop.disarm();
    let json = serde_json::json!({
        "exit_code": status.code(),
        "signal": status.signal(),
        "stdout_base64": STANDARD.encode(stdout),
        "stderr_base64": STANDARD.encode(stderr),
    });
    let bytes =
        serde_json::to_vec(&json).map_err(|error| Error::InvalidConfig(error.to_string()))?;
    let mut response = Response::new(
        Full::new(Bytes::from(bytes))
            .map_err(|error| match error {})
            .boxed_unsync(),
    );
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

async fn read_limited(mut stream: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Ok(output);
        }
        if count > MAX_OUTPUT_BYTES.saturating_sub(output.len()) {
            return Err(Error::BadRequest("exec output exceeds limit".into()));
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

async fn run_command(
    mut child: Child,
    input: Vec<u8>,
    cancel: CancellationToken,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let pid = child.id().map(|id| Pid::from_raw(id as i32));
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let result = {
        let work = async {
            let input = async move {
                match stdin.write_all(&input).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                    Err(error) => return Err(Error::Io(error)),
                }
                drop(stdin);
                Ok::<_, Error>(())
            };
            let ((), stdout, stderr, status) =
                tokio::try_join!(input, read_limited(stdout), read_limited(stderr), async {
                    child.wait().await.map_err(Error::from)
                },)?;
            Ok((status, stdout, stderr))
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            _ = tokio::time::sleep(EXECUTION_TIMEOUT) => Err(Error::Deadline),
            result = work => result,
        }
    };
    if result.is_err() {
        if let Some(pid) = pid {
            let _ = killpg(pid, Signal::SIGTERM);
        }
        if !matches!(
            tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
            Ok(Ok(_))
        ) {
            if let Some(pid) = pid {
                let _ = killpg(pid, Signal::SIGKILL);
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
    result
}

fn empty(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::new())
            .map_err(|error| match error {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response
}

#[cfg(test)]
#[path = "../tests/unit/exec.rs"]
mod tests;

//! Terminal admission and the fixed v1 wire protocol.
//!
//! No execution backend is enabled until its helper, broker, and platform
//! isolation have been implemented and verified.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicU64},
    time::Instant,
};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{Body, Error, Result};

const MAX_SESSIONS: usize = 4;
const MAX_PAYLOAD: usize = 16 * 1024;
const MAX_BUFFER: usize = 16 * (MAX_PAYLOAD + 5);
const VERSION_HEADER: &str = "pishoo-terminal-version";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalPolicy {
    pub enabled: bool,
    pub administrators: HashSet<Arc<str>>,
}

// Keep the frozen resource layout while the actual platform setup is pending.
#[allow(dead_code)]
enum OsBackend {
    Linux {
        install_dir: PathBuf,
        cgroup_root: PathBuf,
    },
    MacOsWasi {
        install_dir: PathBuf,
    },
}

#[allow(dead_code)]
enum TerminalBackend {
    Disabled,
    Unavailable(Error),
    Ready {
        backend: OsBackend,
        run_user: Arc<str>,
        home_root: Arc<File>,
        protected_paths: Arc<[PathBuf]>,
        runtime_dir: PathBuf,
    },
}

pub(crate) struct TerminalManager {
    backend: TerminalBackend,
    administrators: Mutex<HashSet<Arc<str>>>,
    permits: Arc<Semaphore>,
    sessions: Mutex<HashMap<u64, (Arc<str>, CancellationToken)>>,
    #[allow(dead_code)] // Allocated only when an implemented backend can admit a session.
    next_id: AtomicU64,
    tasks: TaskTracker,
}

impl TerminalManager {
    pub fn new(policy: TerminalPolicy, _state_dir: &Path) -> Result<Self> {
        let administrators = policy
            .administrators
            .iter()
            .map(|name| {
                dhttp_home::normalize_name(name)
                    .map(Arc::<str>::from)
                    .ok_or_else(|| {
                        Error::InvalidConfig("invalid terminal administrator name".into())
                    })
            })
            .collect::<Result<HashSet<_>>>()?;
        if policy.enabled && administrators.is_empty() {
            return Err(Error::InvalidConfig(
                "enabled terminal requires an administrator".into(),
            ));
        }
        let backend = if policy.enabled {
            let error = Error::BackendUnavailable(
                "terminal helper, file broker, and platform isolation are not implemented".into(),
            );
            eprintln!("terminal unavailable: {error}");
            TerminalBackend::Unavailable(error)
        } else {
            TerminalBackend::Disabled
        };
        Ok(Self {
            backend,
            administrators: Mutex::new(administrators),
            permits: Arc::new(Semaphore::new(MAX_SESSIONS)),
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            tasks: TaskTracker::new(),
        })
    }

    pub async fn handle(
        self: &Arc<Self>,
        server_name: &str,
        server_cancel: CancellationToken,
        request: Request<dhttp::Body>,
    ) -> Result<Response<Body>> {
        if matches!(self.backend, TerminalBackend::Disabled) {
            return Ok(status(StatusCode::NOT_FOUND));
        }
        let Some(summary) = request.extensions().get::<dhttp::HandshakeSummary>() else {
            return Err(Error::MissingHandshake);
        };
        let Some(local) = summary.local.as_ref() else {
            return Err(Error::IdentityMismatch);
        };
        let Some(server_name) = dhttp_home::normalize_name(server_name) else {
            return Err(Error::IdentityMismatch);
        };
        if dhttp_home::normalize_name(local.name()).as_deref() != Some(server_name.as_str()) {
            return Err(Error::IdentityMismatch);
        }
        let Some(caller) = summary
            .remote
            .as_ref()
            .and_then(|remote| dhttp_home::normalize_name(remote.name()))
        else {
            return Err(Error::Denied);
        };
        if !self
            .administrators
            .lock()
            .unwrap()
            .contains(caller.as_str())
        {
            return Err(Error::Denied);
        }
        if self.permits.is_closed() || server_cancel.is_cancelled() {
            return Err(Error::Closed);
        }
        if request.method() != Method::CONNECT {
            let mut response = status(StatusCode::METHOD_NOT_ALLOWED);
            response
                .headers_mut()
                .insert(http::header::ALLOW, "CONNECT".parse().unwrap());
            return Ok(response);
        }
        let versions = request.headers().get_all(VERSION_HEADER);
        let mut versions = versions.iter();
        if versions.next().map(|value| value.as_bytes()) != Some(b"1")
            || versions.next().is_some()
            || request.extensions().get::<Arc<str>>().map(AsRef::as_ref) != Some("pishoo-terminal")
        {
            return Ok(status(StatusCode::BAD_REQUEST));
        }
        if request.uri().path() != "/shell" && !request.uri().path().starts_with("/shell/") {
            return Ok(status(StatusCode::NOT_FOUND));
        }
        // Never return 200 or start a host process for an unverified backend.
        Ok(status(StatusCode::NOT_IMPLEMENTED))
    }

    pub fn revoke(&self, identity: &str) {
        let Some(identity) = dhttp_home::normalize_name(identity) else {
            return;
        };
        let mut administrators = self.administrators.lock().unwrap();
        let sessions = self.sessions.lock().unwrap();
        administrators.remove(identity.as_str());
        for (caller, cancel) in sessions.values() {
            if caller.as_ref() == identity {
                cancel.cancel();
            }
        }
    }

    pub async fn shutdown(&self, deadline: Instant) -> Result<()> {
        {
            let sessions = self.sessions.lock().unwrap();
            self.permits.close();
            for (_, cancel) in sessions.values() {
                cancel.cancel();
            }
            self.tasks.close();
        }
        tokio::time::timeout_at(deadline.into(), self.tasks.wait())
            .await
            .map_err(|_| Error::ShutdownDeadline)?;
        Ok(())
    }
}

fn status(code: StatusCode) -> Response<Body> {
    let mut response = Response::new(
        Empty::<Bytes>::new()
            .map_err(|error| match error {})
            .boxed_unsync(),
    );
    *response.status_mut() = code;
    response
}

fn invalid(message: &'static str) -> Error {
    Error::BadRequest(message.into())
}

#[allow(dead_code)] // Used by session.run when the execution backend is implemented.
enum TerminalInput {
    Data(dhttp::Body),
    Controls(dhttp::Body),
    Ended,
}

impl TerminalInput {
    #[allow(dead_code)]
    async fn read_next(&mut self, buffer: &mut BytesMut) -> Result<Option<Frame>> {
        let result = async {
            loop {
                if matches!(self, Self::Ended) {
                    return Ok(None);
                }
                if let Some(frame) = Frame::decode(buffer)? {
                    if matches!(
                        frame,
                        Frame::Ready { .. }
                            | Frame::Output(_)
                            | Frame::Exit { .. }
                            | Frame::Error { .. }
                    ) {
                        return Err(invalid("terminal frame has the wrong direction"));
                    }
                    if matches!(self, Self::Controls(_))
                        && !matches!(
                            frame,
                            Frame::Resize { .. } | Frame::Signal(_) | Frame::Cancel
                        )
                    {
                        return Err(invalid("terminal input already ended"));
                    }
                    if matches!(frame, Frame::InputEnd) {
                        let Self::Data(body) = std::mem::replace(self, Self::Ended) else {
                            unreachable!("only Data accepts INPUT_END")
                        };
                        *self = Self::Controls(body);
                    }
                    return Ok(Some(frame));
                }
                let body = match self {
                    Self::Data(body) | Self::Controls(body) => body,
                    Self::Ended => return Ok(None),
                };
                match body.frame().await {
                    Some(Ok(frame)) => {
                        let data = frame
                            .into_data()
                            .map_err(|_| invalid("terminal request trailers are unsupported"))?;
                        if data.len() > MAX_BUFFER.saturating_sub(buffer.len()) {
                            return Err(invalid("terminal input buffer limit exceeded"));
                        }
                        buffer.extend_from_slice(&data);
                    }
                    Some(Err(error)) => return Err(invalid_body(error)),
                    None => {
                        *self = Self::Ended;
                        if !buffer.is_empty() {
                            return Err(invalid("terminal frame was truncated by EOF"));
                        }
                        return Ok(None);
                    }
                }
            }
        }
        .await;
        if result.is_err() {
            *self = Self::Ended;
            buffer.clear();
        }
        result
    }
}

fn invalid_body(error: dhttp::BoxError) -> Error {
    std::io::Error::other(error).into()
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    Open {
        rows: u16,
        cols: u16,
        term: String,
        cwd: String,
    },
    Input(Bytes),
    Resize {
        rows: u16,
        cols: u16,
    },
    InputEnd,
    Signal(u8),
    Cancel,
    Ready {
        session_id: u64,
        mode: u8,
    },
    Output(Bytes),
    Exit {
        kind: u8,
        value: u32,
    },
    Error {
        code: u16,
        message: String,
    },
}

impl Frame {
    #[allow(dead_code)]
    fn encode(&self, output: &mut BytesMut) -> Result<()> {
        let mut payload = BytesMut::new();
        let kind = match self {
            Self::Open {
                rows,
                cols,
                term,
                cwd,
            } => {
                validate_open(*rows, *cols, term, cwd)?;
                payload.put_u16(*rows);
                payload.put_u16(*cols);
                payload.put_u8(term.len() as u8);
                payload.extend_from_slice(term.as_bytes());
                payload.put_u16(cwd.len() as u16);
                payload.extend_from_slice(cwd.as_bytes());
                0x01
            }
            Self::Input(data) | Self::Output(data) => {
                if data.len() > MAX_PAYLOAD {
                    return Err(invalid("terminal frame is too large"));
                }
                payload.extend_from_slice(data);
                if matches!(self, Self::Input(_)) {
                    0x02
                } else {
                    0x11
                }
            }
            Self::Resize { rows, cols } => {
                validate_size(*rows, *cols)?;
                payload.put_u16(*rows);
                payload.put_u16(*cols);
                0x03
            }
            Self::InputEnd => 0x04,
            Self::Signal(signal) => {
                if !(1..=3).contains(signal) {
                    return Err(invalid("invalid terminal signal"));
                }
                payload.put_u8(*signal);
                0x05
            }
            Self::Cancel => 0x06,
            Self::Ready { session_id, mode } => {
                if *mode > 1 {
                    return Err(invalid("invalid terminal mode"));
                }
                payload.put_u64(*session_id);
                payload.put_u8(*mode);
                0x10
            }
            Self::Exit { kind, value } => {
                validate_exit(*kind, *value)?;
                payload.put_u8(*kind);
                payload.put_u32(*value);
                0x12
            }
            Self::Error { code, message } => {
                if !(1..=4).contains(code) || message.len() > 1024 {
                    return Err(invalid("invalid terminal error frame"));
                }
                payload.put_u16(*code);
                payload.extend_from_slice(message.as_bytes());
                0x13
            }
        };
        output.reserve(5 + payload.len());
        output.put_u8(kind);
        output.put_u32(payload.len() as u32);
        output.extend_from_slice(&payload);
        Ok(())
    }

    #[allow(dead_code)]
    fn decode(input: &mut BytesMut) -> Result<Option<Self>> {
        if input.len() < 5 {
            return Ok(None);
        }
        let kind = input[0];
        let length = u32::from_be_bytes(input[1..5].try_into().unwrap()) as usize;
        if length > MAX_PAYLOAD {
            return Err(invalid("terminal frame is too large"));
        }
        let valid_length = match kind {
            0x01 => (7..=1063).contains(&length),
            0x02 | 0x11 => true,
            0x03 => length == 4,
            0x04 | 0x06 => length == 0,
            0x05 => length == 1,
            0x10 => length == 9,
            0x12 => length == 5,
            0x13 => (2..=1026).contains(&length),
            _ => return Err(invalid("unknown terminal frame type")),
        };
        if !valid_length {
            return Err(invalid("invalid terminal payload length"));
        }
        if input.len() < 5 + length {
            return Ok(None);
        }
        let payload = &input[5..5 + length];
        let frame = match kind {
            0x01 => {
                let rows = u16::from_be_bytes(payload[..2].try_into().unwrap());
                let cols = u16::from_be_bytes(payload[2..4].try_into().unwrap());
                let term_length = payload[4] as usize;
                if term_length > 32 || payload.len() < 7 + term_length {
                    return Err(invalid("invalid terminal OPEN strings"));
                }
                let term = std::str::from_utf8(&payload[5..5 + term_length])
                    .map_err(|_| invalid("invalid terminal TERM encoding"))?;
                let cwd_length = u16::from_be_bytes(
                    payload[5 + term_length..7 + term_length]
                        .try_into()
                        .unwrap(),
                ) as usize;
                if payload.len() != 7 + term_length + cwd_length {
                    return Err(invalid("invalid terminal OPEN strings"));
                }
                let cwd = std::str::from_utf8(&payload[7 + term_length..])
                    .map_err(|_| invalid("invalid terminal cwd encoding"))?;
                validate_open(rows, cols, term, cwd)?;
                Self::Open {
                    rows,
                    cols,
                    term: term.into(),
                    cwd: cwd.into(),
                }
            }
            0x02 => Self::Input(Bytes::copy_from_slice(payload)),
            0x03 => {
                let rows = u16::from_be_bytes(payload[..2].try_into().unwrap());
                let cols = u16::from_be_bytes(payload[2..].try_into().unwrap());
                validate_size(rows, cols)?;
                Self::Resize { rows, cols }
            }
            0x04 => Self::InputEnd,
            0x05 => {
                if !(1..=3).contains(&payload[0]) {
                    return Err(invalid("invalid terminal signal"));
                }
                Self::Signal(payload[0])
            }
            0x06 => Self::Cancel,
            0x10 => {
                if payload[8] > 1 {
                    return Err(invalid("invalid terminal mode"));
                }
                Self::Ready {
                    session_id: u64::from_be_bytes(payload[..8].try_into().unwrap()),
                    mode: payload[8],
                }
            }
            0x11 => Self::Output(Bytes::copy_from_slice(payload)),
            0x12 => {
                let value = u32::from_be_bytes(payload[1..].try_into().unwrap());
                validate_exit(payload[0], value)?;
                Self::Exit {
                    kind: payload[0],
                    value,
                }
            }
            0x13 => {
                let code = u16::from_be_bytes(payload[..2].try_into().unwrap());
                if !(1..=4).contains(&code) {
                    return Err(invalid("invalid terminal error code"));
                }
                let message = std::str::from_utf8(&payload[2..])
                    .map_err(|_| invalid("invalid terminal error encoding"))?;
                Self::Error {
                    code,
                    message: message.into(),
                }
            }
            _ => unreachable!(),
        };
        input.advance(5 + length);
        Ok(Some(frame))
    }
}

fn validate_size(rows: u16, cols: u16) -> Result<()> {
    if !(1..=1000).contains(&rows) || !(1..=1000).contains(&cols) {
        return Err(invalid("terminal dimensions must be in 1..=1000"));
    }
    Ok(())
}

fn validate_open(rows: u16, cols: u16, term: &str, cwd: &str) -> Result<()> {
    validate_size(rows, cols)?;
    if !matches!(term, "xterm-256color" | "xterm" | "vt100" | "dumb") {
        return Err(invalid("unsupported terminal TERM"));
    }
    if cwd.len() > 1024 || cwd.contains(['\0', '\\', '%', ':']) {
        return Err(invalid("invalid terminal cwd"));
    }
    if !cwd.is_empty()
        && cwd
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.len() > 255)
    {
        return Err(invalid("terminal cwd must be a canonical relative path"));
    }
    if matches!(
        cwd.split('/').next().unwrap_or(""),
        ".profile"
            | ".bash_profile"
            | ".bashrc"
            | ".zshenv"
            | ".zprofile"
            | ".zshrc"
            | ".ssh"
            | ".aws"
            | ".gnupg"
            | ".config"
            | "Library"
    ) {
        return Err(invalid("terminal cwd is reserved"));
    }
    Ok(())
}

fn validate_exit(kind: u8, value: u32) -> Result<()> {
    if !match kind {
        0 => value <= 255,
        1 => value > 0,
        2 => (1..=6).contains(&value),
        _ => false,
    } {
        return Err(invalid("invalid terminal exit status"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use http_body_util::{Full, StreamBody};

    use super::*;

    fn body(bytes: Bytes) -> Body {
        Full::new(bytes)
            .map_err(|error| match error {})
            .boxed_unsync()
    }

    fn encoded(frames: &[Frame]) -> Bytes {
        let mut bytes = BytesMut::new();
        for frame in frames {
            frame.encode(&mut bytes).unwrap();
        }
        bytes.freeze()
    }

    #[test]
    fn policy_defaults_and_unknown_fields() {
        let policy: TerminalPolicy = serde_json::from_str("{}").unwrap();
        assert!(!policy.enabled);
        assert!(policy.administrators.is_empty());
        assert!(
            serde_json::from_str::<TerminalPolicy>(r#"{"enabled":false,"run_user":"root"}"#)
                .is_err()
        );
        let manager = TerminalManager::new(policy, Path::new("/does/not/exist")).unwrap();
        assert!(matches!(manager.backend, TerminalBackend::Disabled));
    }

    #[test]
    fn administrator_names_are_normalized_and_unavailable_backend_stays_closed() {
        let manager = TerminalManager::new(
            TerminalPolicy {
                enabled: true,
                administrators: [Arc::from(" Alice "), Arc::from("alice.dhttp.net")].into(),
            },
            Path::new("/does/not/exist"),
        )
        .unwrap();
        assert!(matches!(
            manager.backend,
            TerminalBackend::Unavailable(Error::BackendUnavailable(_))
        ));
        let administrators = manager.administrators.lock().unwrap();
        assert_eq!(administrators.len(), 1);
        assert!(administrators.contains("alice.dhttp.net"));
        assert!(
            TerminalManager::new(
                TerminalPolicy {
                    enabled: true,
                    administrators: HashSet::new()
                },
                Path::new(".")
            )
            .is_err()
        );
        assert!(
            TerminalManager::new(
                TerminalPolicy {
                    enabled: true,
                    administrators: [Arc::from("../alice")].into()
                },
                Path::new(".")
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn disabled_terminal_is_hidden_and_missing_handshake_is_an_error() {
        let manager =
            Arc::new(TerminalManager::new(TerminalPolicy::default(), Path::new(".")).unwrap());
        let response = manager
            .handle(
                "alice",
                CancellationToken::new(),
                Request::new(body(Bytes::new())),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let manager = Arc::new(
            TerminalManager::new(
                TerminalPolicy {
                    enabled: true,
                    administrators: [Arc::from("alice")].into(),
                },
                Path::new("."),
            )
            .unwrap(),
        );
        assert!(matches!(
            manager
                .handle(
                    "alice",
                    CancellationToken::new(),
                    Request::new(body(Bytes::new()))
                )
                .await,
            Err(Error::MissingHandshake)
        ));
    }

    #[tokio::test]
    async fn revocation_and_shutdown_cancel_only_owned_sessions() {
        let manager = TerminalManager::new(
            TerminalPolicy {
                enabled: true,
                administrators: [Arc::from("alice"), Arc::from("bob")].into(),
            },
            Path::new("."),
        )
        .unwrap();
        let server = CancellationToken::new();
        let alice = server.child_token();
        let bob = server.child_token();
        manager
            .sessions
            .lock()
            .unwrap()
            .insert(1, (Arc::from("alice.dhttp.net"), alice.clone()));
        manager
            .sessions
            .lock()
            .unwrap()
            .insert(2, (Arc::from("bob.dhttp.net"), bob.clone()));
        manager.revoke(" Alice ");
        assert!(alice.is_cancelled());
        assert!(!bob.is_cancelled());
        assert!(!server.is_cancelled());
        assert!(
            !manager
                .administrators
                .lock()
                .unwrap()
                .contains("alice.dhttp.net")
        );
        manager
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        assert!(bob.is_cancelled());
        assert!(!server.is_cancelled());
        assert!(manager.permits.is_closed());
        manager
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_deadline_does_not_abort_owned_cleanup() {
        let manager = TerminalManager::new(TerminalPolicy::default(), Path::new(".")).unwrap();
        let (finish, waiting) = tokio::sync::oneshot::channel();
        let cleaning = manager.tasks.spawn(async move {
            waiting.await.unwrap();
        });
        assert!(matches!(
            manager.shutdown(Instant::now()).await,
            Err(Error::ShutdownDeadline)
        ));
        assert!(!cleaning.is_finished());
        finish.send(()).unwrap();
        cleaning.await.unwrap();
        manager
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[test]
    fn fixed_wire_values_and_fragmented_open_are_unambiguous() {
        let frame = Frame::Open {
            rows: 24,
            cols: 80,
            term: "xterm".into(),
            cwd: "work".into(),
        };
        let wire = encoded(&[frame]);
        assert_eq!(
            &wire[..],
            &[
                1, 0, 0, 0, 16, 0, 24, 0, 80, 5, b'x', b't', b'e', b'r', b'm', 0, 4, b'w', b'o',
                b'r', b'k'
            ]
        );
        for split in 0..wire.len() {
            let mut buffer = BytesMut::from(&wire[..split]);
            assert!(Frame::decode(&mut buffer).unwrap().is_none());
            assert_eq!(buffer.as_ref(), &wire[..split]);
            buffer.extend_from_slice(&wire[split..]);
            assert_eq!(
                Frame::decode(&mut buffer).unwrap(),
                Some(Frame::Open {
                    rows: 24,
                    cols: 80,
                    term: "xterm".into(),
                    cwd: "work".into()
                })
            );
            assert!(buffer.is_empty());
        }
        assert_eq!(
            encoded(&[Frame::Resize {
                rows: 1,
                cols: 1000
            }])
            .as_ref(),
            &[3, 0, 0, 0, 4, 0, 1, 3, 232]
        );
        assert_eq!(
            encoded(&[Frame::Exit { kind: 2, value: 6 }]).as_ref(),
            &[0x12, 0, 0, 0, 5, 2, 0, 0, 0, 6]
        );
    }

    #[test]
    fn arbitrary_data_and_each_control_frame_round_trip() {
        let frames = [
            Frame::Input(Bytes::from_static(&[0, 255, 128])),
            Frame::Resize {
                rows: 1000,
                cols: 1,
            },
            Frame::InputEnd,
            Frame::Signal(3),
            Frame::Cancel,
            Frame::Ready {
                session_id: u64::MAX,
                mode: 1,
            },
            Frame::Output(Bytes::from_static(&[255, 0])),
            Frame::Exit {
                kind: 0,
                value: 255,
            },
            Frame::Error {
                code: 1,
                message: "bad input".into(),
            },
        ];
        let mut bytes = BytesMut::from(encoded(&frames).as_ref());
        for frame in frames {
            assert_eq!(Frame::decode(&mut bytes).unwrap(), Some(frame));
        }
        assert!(bytes.is_empty());
    }

    #[test]
    fn oversize_unknown_and_invalid_control_values_fail_before_consumption() {
        for wire in [
            &[2, 0, 0, 64, 1][..],
            &[0xff, 0, 0, 0, 0],
            &[4, 0, 0, 0, 1],
            &[3, 0, 0, 0, 4, 0, 0, 0, 1],
            &[5, 0, 0, 0, 1, 4],
            &[0x10, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 1, 2],
            &[0x12, 0, 0, 0, 5, 0, 0, 0, 1, 0],
            &[0x12, 0, 0, 0, 5, 2, 0, 0, 0, 7],
            &[0x13, 0, 0, 0, 3, 0, 1, 255],
        ] {
            let mut buffer = BytesMut::from(wire);
            assert!(Frame::decode(&mut buffer).is_err(), "{wire:?}");
            assert_eq!(buffer.as_ref(), wire);
        }
        let mut output = BytesMut::from(&b"prior"[..]);
        assert!(Frame::Signal(0).encode(&mut output).is_err());
        assert_eq!(output.as_ref(), b"prior");
    }

    #[test]
    fn open_rejects_ambiguous_and_reserved_paths_and_unsupported_terms() {
        for cwd in [
            "/etc",
            "..",
            "a/../b",
            "a//b",
            "a/./b",
            "a/",
            "a\\b",
            "%2e%2e",
            "C:work",
            "\0",
            ".ssh/key",
            ".config",
            "Library/LaunchAgents",
        ] {
            assert!(validate_open(24, 80, "xterm", cwd).is_err(), "{cwd:?}");
        }
        assert!(validate_open(24, 80, "screen", "").is_err());
        assert!(validate_open(24, 80, "xterm", &"a".repeat(1025)).is_err());
        assert!(validate_open(24, 80, "xterm", "notes/会议").is_ok());
    }

    #[tokio::test]
    async fn input_end_preserves_controls_until_http_eof() {
        let mut input = TerminalInput::Data(body(encoded(&[
            Frame::InputEnd,
            Frame::Resize {
                rows: 30,
                cols: 100,
            },
            Frame::Signal(1),
            Frame::Cancel,
        ])));
        let mut buffer = BytesMut::new();
        assert_eq!(
            input.read_next(&mut buffer).await.unwrap(),
            Some(Frame::InputEnd)
        );
        assert!(matches!(input, TerminalInput::Controls(_)));
        assert_eq!(
            input.read_next(&mut buffer).await.unwrap(),
            Some(Frame::Resize {
                rows: 30,
                cols: 100
            })
        );
        assert_eq!(
            input.read_next(&mut buffer).await.unwrap(),
            Some(Frame::Signal(1))
        );
        assert_eq!(
            input.read_next(&mut buffer).await.unwrap(),
            Some(Frame::Cancel)
        );
        assert!(input.read_next(&mut buffer).await.unwrap().is_none());
        assert!(matches!(input, TerminalInput::Ended));
        assert!(input.read_next(&mut buffer).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn invalid_direction_repeated_end_and_truncation_release_input() {
        for bytes in [
            encoded(&[Frame::InputEnd, Frame::Input(Bytes::from_static(b"late"))]),
            encoded(&[Frame::InputEnd, Frame::InputEnd]),
            encoded(&[Frame::Output(Bytes::new())]),
            Bytes::from_static(&[1, 0, 0]),
        ] {
            let mut input = TerminalInput::Data(body(bytes));
            let mut buffer = BytesMut::new();
            loop {
                match input.read_next(&mut buffer).await {
                    Err(_) => break,
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("malformed input must fail"),
                }
            }
            assert!(matches!(input, TerminalInput::Ended));
            assert!(buffer.is_empty());
            assert!(input.read_next(&mut buffer).await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn body_failure_is_not_an_eof() {
        let frames = futures::stream::iter([Err::<http_body::Frame<Bytes>, dhttp::BoxError>(
            Box::new(std::io::Error::other("transport reset")),
        )]);
        let mut input = TerminalInput::Data(StreamBody::new(frames).boxed_unsync());
        assert!(input.read_next(&mut BytesMut::new()).await.is_err());
        assert!(matches!(input, TerminalInput::Ended));
    }
}

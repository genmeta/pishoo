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

fn invalid(message: &'static str) -> Error {
    Error::BadRequest(message.into())
}

include!("terminal/manager.rs");
include!("terminal/input.rs");
include!("terminal/protocol.rs");

#[cfg(test)]
#[path = "../tests/unit/terminal/mod.rs"]
mod tests;

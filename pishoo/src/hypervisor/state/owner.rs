#![allow(dead_code)]

use nix::unistd::{Pid, Uid};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owner {
    Local,
    Worker { uid: Uid, pid: Pid },
}

impl Owner {
    pub const fn worker(uid: Uid, pid: Pid) -> Self {
        Self::Worker { uid, pid }
    }

    pub const fn pid(self) -> Option<Pid> {
        match self {
            Self::Local => None,
            Self::Worker { pid, .. } => Some(pid),
        }
    }

    pub const fn uid(self) -> Option<Uid> {
        match self {
            Self::Local => None,
            Self::Worker { uid, .. } => Some(uid),
        }
    }
}

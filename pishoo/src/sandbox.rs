//! WASM execution resources shared by one identity's Libs and Router versions.

use std::{sync::Arc, time::Duration};

use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

use crate::{Error, Result};

pub(crate) struct Sandbox {
    pub(crate) lib_slots: Arc<Semaphore>,
    pub(crate) tasks: TaskTracker,
}

impl Sandbox {
    pub(crate) fn new() -> Self {
        Self {
            lib_slots: Arc::new(Semaphore::new(4)),
            tasks: TaskTracker::new(),
        }
    }

    /// Stop admitting executions. Server owns cancellation of the identity.
    pub(crate) fn close(&self) {
        self.lib_slots.close();
        self.tasks.close();
    }

    /// Wait for actual Store/task disposal after Server cancels the identity.
    /// A timeout leaves the tracked tasks owning their resources.
    pub(crate) async fn wait(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), self.tasks.wait())
            .await
            .map_err(|_| Error::ShutdownDeadline)
    }
}

#[cfg(test)]
#[path = "../tests/unit/sandbox.rs"]
mod tests;

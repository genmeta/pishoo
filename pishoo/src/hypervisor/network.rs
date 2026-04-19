//! Network interface watch loop.
//!
//! Each registered server owns its own per-SNI accept task (spawned inside
//! [`RootState::register_listener`]) that drains the shared
//! `ServerBinding` into the worker's mpsc channel, so there is no longer
//! a single central accept loop. What remains here is the background task
//! that reconciles per-server bind URIs in response to interface changes
//! reported by [`Devices`].

use std::sync::Arc;

use h3x::dquic::qinterface::device::Devices;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;

use super::state::RootState;

/// Watch for network interface changes and reconcile bind URIs.
///
/// Runs forever (until dropped/aborted). On each interface event,
/// re-resolves all active servers' `Listens` against the current
/// device list and applies bind/unbind as needed.
async fn run_network_watch_loop(state: Arc<RootState>) {
    let mut monitor = Devices::global().monitor();
    while let Some((_interfaces, event)) = monitor.update().await {
        tracing::debug!(?event, "network interface change detected");
        state.reconcile_binds(&event).await;
    }
}

/// Spawn the network watch loop as a background task. Returns the join handle.
pub fn spawn_network_watch_loop(state: Arc<RootState>) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(
        run_network_watch_loop(state).in_current_span(),
    ))
}

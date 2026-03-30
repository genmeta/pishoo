//! QUIC listeners/client management and connection routing.
//!
//! The root process runs a single [`QuicListeners`] that multiplexes all
//! servers. This module provides the central accept loop that routes
//! incoming connections by `server_name` to per-server mpsc channels.

use std::sync::Arc;

use snafu::Report;
use tracing::Instrument;

use super::state::RootState;

/// Run the central accept loop: route connections by server_name.
///
/// This task runs forever (until the listeners are dropped or an
/// unrecoverable error occurs). Each incoming connection is dispatched
/// to the per-server mpsc channel registered in [`RootState`].
pub async fn run_accept_loop(state: Arc<RootState>) {
    loop {
        let (conn, server_name, _pathway, _link) = {
            match state.listeners.accept().await {
                Ok(incoming) => incoming,
                Err(error) => {
                    tracing::error!(
                        error = %Report::from_error(&error),
                        "Accept loop error"
                    );
                    break;
                }
            }
        };

        let sender = state.get_conn_sender(&server_name).await;
        let Some(sender) = sender else {
            tracing::warn!(%server_name, "No listener registered for connection");
            continue;
        };

        if sender.send(conn).await.is_err() {
            tracing::warn!(%server_name, "Failed to route connection (channel closed)");
        }
    }
}

/// Spawn the accept loop as a background task. Returns the join handle.
pub fn spawn_accept_loop(state: Arc<RootState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_accept_loop(state).in_current_span())
}

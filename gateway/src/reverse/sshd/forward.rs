use std::sync::Arc;

use futures::{FutureExt, StreamExt};
pub use ssh3_proto::forward::*;
use ssh3_proto::{
    listener,
    messages::{self, BindAddress},
    mux::{Mux, Recver, Sender, Token},
};
use tokio::io;

use super::Error;

pub async fn listen_remote_forward(
    mux: Arc<Mux>,
    token: Token,
    mut sender: Sender,
    mut recver: Recver,
    listen: BindAddress,
) -> Result<(), Error> {
    let remote_forwarder = Forwarder::new(
        mux.clone(),
        messages::OpenChannel::Forwarded {
            listen: token,
            to: None,
        },
    );
    let listener = match listener::Listener::bind(listen.clone()).await {
        Ok(listener) => {
            tracing::info!("Listening on {listen}");
            listener
        }
        Err(error) => {
            _ = sender
                .cancel(io::Error::other(format!(
                    "Peer failed to bind {listen}: {error:?}"
                )))
                .await;
            return Ok(());
        }
    };
    let listen_task =
        listener.listen(move |reader, writer| remote_forwarder.forward(reader, writer).boxed());
    tokio::select! {
        _ = recver.next() => Ok(()),
        error = listen_task => Err(error),

    }
}

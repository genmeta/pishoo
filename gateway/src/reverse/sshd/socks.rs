use std::{io, sync::Arc};

use futures::{FutureExt, StreamExt};
use ssh3_proto::{
    listener,
    messages::BindAddress,
    mux::{Mux, Recver, Sender, Token},
    socks,
};

use super::Error;

pub async fn listen_remote_forward(
    mux: Arc<Mux>,
    token: Token,
    mut sender: Sender,
    mut recver: Recver,
    listen: BindAddress,
) -> Result<(), Error> {
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
    tokio::select! {
        _ = recver.next() => Ok(()),
        error = listener.listen(move |reader, writer| {
            let mux = mux.clone();
            async move {
                socks::accept_forward(reader, writer, mux, token).await
            }.boxed()
        }) => Err(error.into()),

    }
}

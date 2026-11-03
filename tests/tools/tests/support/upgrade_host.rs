//! Bind the real host to a fixed loopback test socket; discovery remains DNS.
use std::{error::Error, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    dhttp::DhttpNetwork::init().await?;
    let socket=Arc::new(qprotocol::UdpSocket::bind(([127,0,0,1],13445).into())?);
    qprotocol::Dock::global().add(socket.clone())?.ok_or("test host socket already registered")?;
    let _socket=scopeguard::guard(socket,|socket|{qprotocol::Dock::global().remove(&socket);});
    pishoo::run().await?;
    Ok(())
}

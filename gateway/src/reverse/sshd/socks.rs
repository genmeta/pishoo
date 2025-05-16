// //! Socks5 proxy server implementation.

use std::{fmt::Debug, sync::Arc};

use bytes::Bytes;
use dashmap::DashMap;
use futures::{Sink, SinkExt, StreamExt, channel::mpsc};
use serde::{Deserialize, Serialize};
use socks5_proto::{Address, Error, ProtocolError, Reply, Request, Response, handshake};
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_util::{
    io::{CopyToBytes, SinkWriter, StreamReader},
    task::AbortOnDropHandle,
};
use tracing::Instrument;

use super::map_sink::MapSinkExt;

pub type Token = u64;

#[derive(Deserialize, Debug)]
pub enum ClientSocksMessage {
    Init { token: Token },
    Data { token: Token, data: Bytes },
    Finish { token: Token },
}

#[derive(Serialize, Debug)]
pub enum ServerSocksMessage {
    Data { token: Token, data: Bytes },
    Error { token: Token, error: String },
}

pub struct SocksConnection {
    data_recver: mpsc::Sender<Bytes>,
    _task_handle: AbortOnDropHandle<()>,
}

#[derive(Default)]
pub struct SocksServer {
    connections: Arc<DashMap<Token, SocksConnection>>,
}

impl SocksServer {
    pub fn accpet<S>(&self, token: Token, mut message_sender: S)
    where
        S: Sink<ServerSocksMessage, Error: Debug + Send> + Clone + Send + Unpin + 'static,
    {
        let connections = self.connections.clone();
        let (data_recver, rcvd_data_stream) = mpsc::channel(16);

        let handle_connection = async move {
            let mut reader = StreamReader::new(rcvd_data_stream.map(io::Result::Ok));
            let data_sender = message_sender
                .clone()
                .mapped(|data: Bytes| Ok(ServerSocksMessage::Data { token, data }))
                .sink_map_err(|send_error| {
                    io::Error::other(format!("Server internal send error: {send_error:?}"))
                });
            let mut writer = SinkWriter::new(CopyToBytes::new(data_sender));

            tracing::info!(target: "socks", token, "Accpet connection");
            if let Err(error) = accpet(&mut reader, &mut writer).await {
                tracing::warn!(target: "socks", "Failed to server socks: {error:?}");
                let _ = message_sender
                    .send(ServerSocksMessage::Error {
                        token,
                        error: format!("{error:?}"),
                    })
                    .await;
            }
            connections.remove(&token);
            tracing::info!(target: "socks", token, "Connection closed");
        };

        let connection = SocksConnection {
            data_recver,
            _task_handle: AbortOnDropHandle::new(tokio::spawn(handle_connection.in_current_span())),
        };
        self.connections.insert(token, connection);
    }

    pub async fn receive(&self, token: Token, data: Bytes) {
        if let Some(mut connection) = self.connections.get_mut(&token) {
            if let Err(error) = connection.data_recver.send(data).await {
                tracing::warn!(target: "socks", token, "Failed to send data to connection: {error:?}");
            }
        } else {
            tracing::warn!(target: "socks", token, "Connection not found");
        }
    }

    pub fn close(&self, token: Token) {
        self.connections.remove(&token);
    }
}

impl Drop for SocksServer {
    fn drop(&mut self) {
        self.connections.clear();
    }
}

pub async fn accpet(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
) -> io::Result<()> {
    let handshake_request = match handshake::Request::read_from(reader).await {
        Ok(handshake_request) => handshake_request,
        Err(error) => {
            tracing::warn!(target: "socks", "Failed to parse handshake request: {error:?}");
            return Err(error.into());
        }
    };

    if handshake_request.methods.contains(&handshake::Method::NONE) {
        handshake::Response::new(handshake::Method::NONE)
            .write_to(writer)
            .await?;
    } else {
        tracing::warn!(target: "socks", "No acceptable method, reject handshake request");
        handshake::Response::new(handshake::Method::UNACCEPTABLE)
            .write_to(writer)
            .await?;
        return Err(Error::Protocol(
            ProtocolError::NoAcceptableHandshakeMethod {
                version: socks5_proto::SOCKS_VERSION,
                chosen_method: handshake::Method::NONE,
                methods: handshake_request.methods,
            },
        ))?;
    }

    let request = match Request::read_from(reader).await {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(target: "socks", "Failed to parse request: {error:?}");
            Response::new(Reply::GeneralFailure, Address::unspecified())
                .write_to(writer)
                .await?;
            return Err(error.into());
        }
    };

    match request.command {
        socks5_proto::Command::Connect => {
            let connect = match request.address {
                Address::SocketAddress(socket_addr) => TcpStream::connect(socket_addr).await,
                Address::DomainAddress(ref domain, port) => {
                    let domain = String::from_utf8_lossy(domain);
                    TcpStream::connect((domain.as_ref(), port)).await
                }
            };
            let mut tcp_stream = match connect {
                Ok(tcp_stream) => tcp_stream,
                Err(error) => {
                    let reply = match error.kind() {
                        io::ErrorKind::ConnectionRefused => Reply::ConnectionRefused,
                        io::ErrorKind::NetworkUnreachable => Reply::NetworkUnreachable,
                        io::ErrorKind::HostUnreachable => Reply::HostUnreachable,
                        io::ErrorKind::TimedOut => Reply::NetworkUnreachable,
                        _ => Reply::GeneralFailure,
                    };
                    tracing::warn!(target: "socks", "Failed to connect to {}: {error:?}", request.address);
                    Response::new(reply, Address::unspecified())
                        .write_to(writer)
                        .await?;
                    return Err(error);
                }
            };

            Response::new(Reply::Succeeded, Address::unspecified())
                .write_to(writer)
                .await?;
            tracing::info!(target: "socks", "Connected to {}", request.address);
            io::copy_bidirectional(&mut tcp_stream, &mut io::join(reader, writer)).await?;
            Ok(())
        }
        socks5_proto::Command::Bind | socks5_proto::Command::Associate => {
            tracing::warn!(target: "socks", "BIND and ASSOCIATE commands are not supported");
            Response::new(Reply::CommandNotSupported, Address::unspecified())
                .write_to(writer)
                .await
        }
    }
}

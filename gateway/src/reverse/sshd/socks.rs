// //! Socks5 proxy server implementation.

use socks5_proto::{Address, Error, ProtocolError, Reply, Request, Response, handshake};
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::TcpStream,
};

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
            tracing::info!(target: "socks", "Shutdown connect to {}", request.address);
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

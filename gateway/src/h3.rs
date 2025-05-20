use std::{
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use bytes::{Buf, Bytes};
use derive_more::From;
use futures::{Sink, Stream, future::BoxFuture};
use tokio::io::AsyncWrite;

pub type H3SendRequest = h3::client::SendRequest<h3_shim::OpenStreams, Bytes>;

#[derive(From)]
pub enum H3RecvStream {
    Client(h3::client::RequestStream<h3_shim::RecvStream, Bytes>),
    Server(h3::server::RequestStream<h3_shim::RecvStream, Bytes>),
}

/// A wrapper around a `h3::RequestStream<RecvStream, Bytes>` that implements [`Stream`].
///
/// For [`AsyncRead`], wrapper this in [`tokio_util::io::StreamReader`].
///
/// [`AsyncRead`]: tokio::io::AsyncRead
pub struct H3Stream {
    stream: H3RecvStream,
}

impl H3Stream {
    pub fn new(stream: impl Into<H3RecvStream>) -> H3Stream {
        H3Stream {
            stream: stream.into(),
        }
    }
}

impl Stream for H3Stream {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match &mut self.get_mut().stream {
            H3RecvStream::Client(request_stream) => request_stream
                .poll_recv_data(cx)
                .map_err(io::Error::other)
                .map(|r| r.transpose())
                .map_ok(|mut buf| buf.copy_to_bytes(buf.remaining())),
            H3RecvStream::Server(request_stream) => request_stream
                .poll_recv_data(cx)
                .map_err(io::Error::other)
                .map(|r| r.transpose())
                .map_ok(|mut buf| buf.copy_to_bytes(buf.remaining())),
        }
    }
}

#[derive(From)]
pub enum H3SendStream {
    Client(h3::client::RequestStream<h3_shim::SendStream<Bytes>, Bytes>),
    Server(h3::server::RequestStream<h3_shim::SendStream<Bytes>, Bytes>),
}

/// A wrapper around a `h3::RequestStream<SendStream, Bytes>` that implements [`Sink`] and [`AsyncWrite`].
///
/// Note that [`Sink`] api will always buffer a item, you should flush this or the item will not be sent.  
#[allow(clippy::large_enum_variant)]
pub enum H3Sink {
    Send(BoxFuture<'static, (H3SendStream, io::Result<usize>)>),
    Close(BoxFuture<'static, (H3SendStream, io::Result<()>)>),
    Idle(H3SendStream),
    Invalid,
}

impl H3Sink {
    pub fn new(stream: impl Into<H3SendStream>) -> H3Sink {
        H3Sink::Idle(stream.into())
    }
}

impl Sink<Bytes> for H3Sink {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        match this {
            H3Sink::Send(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3Sink::Close(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3Sink::Idle(_) => Poll::Ready(Ok(())),
            H3Sink::Invalid => {
                unreachable!("H3StreamWriter state error(Invalid in poll_ready)")
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, buf: Bytes) -> Result<(), Self::Error> {
        let this = self.get_mut();
        match this {
            H3Sink::Idle(..) => {
                let mut h3_send_stream = match std::mem::replace(this, Self::Invalid) {
                    H3Sink::Idle(stream) => stream,
                    _ => unreachable!(),
                };
                *this = Self::Send(Box::pin(async move {
                    let result = match &mut h3_send_stream {
                        H3SendStream::Client(stream) => stream.send_data(buf.clone()).await,
                        H3SendStream::Server(stream) => stream.send_data(buf.clone()).await,
                    };
                    let result = result.map(|_| buf.len()).map_err(io::Error::other);
                    (h3_send_stream, result)
                }));
                Ok(())
            }
            H3Sink::Send(..) | H3Sink::Close(..) => {
                unreachable!(
                    "H3StreamWriter state error(Send or Close in start_send, incomplete calling)"
                );
            }
            H3Sink::Invalid => {
                unreachable!("H3StreamWriter state error(Invalid in start_send)");
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        match this {
            H3Sink::Send(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3Sink::Close(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3Sink::Idle(_) => Poll::Ready(Ok(())),
            H3Sink::Invalid => {
                unreachable!("H3StreamWriter state error(flush)");
            }
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        loop {
            match this {
                H3Sink::Send(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    *this = Self::Idle(stream);
                    return Poll::Ready(result.map(|_| ()));
                }
                H3Sink::Close(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    *this = Self::Idle(stream);
                    return Poll::Ready(result.map(|_| ()));
                }
                H3Sink::Idle(..) => {
                    let mut h3_send_stream = match std::mem::replace(this, Self::Invalid) {
                        H3Sink::Idle(stream) => stream,
                        _ => unreachable!(),
                    };
                    *this = Self::Close(Box::pin(async move {
                        let result = match &mut h3_send_stream {
                            H3SendStream::Client(stream) => stream.finish().await,
                            H3SendStream::Server(stream) => stream.finish().await,
                        };
                        (h3_send_stream, result.map_err(io::Error::other))
                    }))
                }
                H3Sink::Invalid => {
                    unreachable!("H3StreamWriter state error(Invalid in shutdown)");
                }
            }
        }
    }
}

impl Sink<&[u8]> for H3Sink {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        <Self as Sink<Bytes>>::poll_ready(self, cx)
    }

    fn start_send(self: Pin<&mut Self>, item: &[u8]) -> Result<(), Self::Error> {
        <Self as Sink<Bytes>>::start_send(self, Bytes::copy_from_slice(item))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        <Self as Sink<Bytes>>::poll_flush(self, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        <Self as Sink<Bytes>>::poll_close(self, cx)
    }
}

impl AsyncWrite for H3Sink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        loop {
            if matches!(*self.as_ref(), Self::Send(..)) {
                ready!(<Self as Sink<Bytes>>::poll_ready(self, cx)?);
                return Poll::Ready(Ok(buf.len()));
            } else {
                self.as_mut().start_send(buf)?;
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        <Self as Sink<&[u8]>>::poll_flush(self, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        <Self as Sink<&[u8]>>::poll_close(self, cx)
    }
}

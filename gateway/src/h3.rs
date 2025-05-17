use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use bytes::{Buf, BufMut, Bytes};
use derive_more::From;
use futures::{Sink, Stream, future::BoxFuture};
use hyper::body::Frame;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub type H3Conn = h3::client::Connection<h3_shim::QuicConnection, Bytes>;
pub type H3SendRequest = h3::client::SendRequest<h3_shim::OpenStreams, Bytes>;

#[derive(From)]
pub enum H3RecvStream<'s> {
    Client(&'s mut h3::client::RequestStream<h3_shim::RecvStream, Bytes>),
    Server(&'s mut h3::server::RequestStream<h3_shim::RecvStream, Bytes>),
}

/// A wrapper around a `h3::RequestStream<RecvStream, Bytes>` that implements [`Stream`].
///
/// For [`AsyncRead`], wrapper this in [`tokio_util::io::StreamReader`].
///
/// [`AsyncRead`]: tokio::io::AsyncRead
pub struct H3StreamReader<'s> {
    stream: H3RecvStream<'s>,
}

impl H3StreamReader<'_> {
    pub fn new<'s>(stream: impl Into<H3RecvStream<'s>>) -> H3StreamReader<'s> {
        H3StreamReader {
            stream: stream.into(),
        }
    }
}

impl Stream for H3StreamReader<'_> {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let r = match &mut self.get_mut().stream {
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
        };
        tracing::trace!(target: "dbg", result =?r, "poll_next called");
        r
    }
}

#[derive(From)]
pub enum H3SendStream<'s> {
    Client(&'s mut h3::client::RequestStream<h3_shim::SendStream<Bytes>, Bytes>),
    Server(&'s mut h3::server::RequestStream<h3_shim::SendStream<Bytes>, Bytes>),
}

/// A wrapper around a `h3::RequestStream<SendStream, Bytes>` that implements [`Sink`] and [`AsyncWrite`].
///
/// Note that [`Sink`] api will always buffer a item, you should flush this or the item will not be sent.  
#[allow(clippy::large_enum_variant)]
pub enum H3StreamWriter<'s> {
    Send(BoxFuture<'s, (H3SendStream<'s>, io::Result<usize>)>),
    Close(BoxFuture<'s, (H3SendStream<'s>, io::Result<()>)>),
    Idle(H3SendStream<'s>),
    Invalid,
}

impl H3StreamWriter<'_> {
    pub fn new<'s>(stream: impl Into<H3SendStream<'s>>) -> H3StreamWriter<'s> {
        H3StreamWriter::Idle(stream.into())
    }
}

impl Sink<Bytes> for H3StreamWriter<'_> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        match this {
            H3StreamWriter::Send(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3StreamWriter::Close(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3StreamWriter::Idle(_) => Poll::Ready(Ok(())),
            H3StreamWriter::Invalid => {
                unreachable!("H3StreamWriter state error(Invalid in poll_ready)")
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, buf: Bytes) -> Result<(), Self::Error> {
        let this = self.get_mut();
        match this {
            H3StreamWriter::Idle(..) => {
                let mut h3_send_stream = match std::mem::replace(this, Self::Invalid) {
                    H3StreamWriter::Idle(stream) => stream,
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
            H3StreamWriter::Send(..) | H3StreamWriter::Close(..) => {
                unreachable!(
                    "H3StreamWriter state error(Send or Close in start_send, incomplete calling)"
                );
            }
            H3StreamWriter::Invalid => {
                unreachable!("H3StreamWriter state error(Invalid in start_send)");
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        match this {
            H3StreamWriter::Send(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3StreamWriter::Close(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3StreamWriter::Idle(_) => Poll::Ready(Ok(())),
            H3StreamWriter::Invalid => {
                unreachable!("H3StreamWriter state error(flush)");
            }
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        loop {
            match this {
                H3StreamWriter::Send(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    *this = Self::Idle(stream);
                    return Poll::Ready(result.map(|_| ()));
                }
                H3StreamWriter::Close(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    *this = Self::Idle(stream);
                    return Poll::Ready(result.map(|_| ()));
                }
                H3StreamWriter::Idle(..) => {
                    let mut h3_send_stream = match std::mem::replace(this, Self::Invalid) {
                        H3StreamWriter::Idle(stream) => stream,
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
                H3StreamWriter::Invalid => {
                    unreachable!("H3StreamWriter state error(Invalid in shutdown)");
                }
            }
        }
    }
}

impl Sink<&[u8]> for H3StreamWriter<'_> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <Self as Sink<Bytes>>::poll_ready(self, cx)
    }

    fn start_send(self: Pin<&mut Self>, item: &[u8]) -> Result<(), Self::Error> {
        <Self as Sink<Bytes>>::start_send(self, Bytes::copy_from_slice(item))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <Self as Sink<Bytes>>::poll_flush(self, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <Self as Sink<Bytes>>::poll_close(self, cx)
    }
}

impl AsyncWrite for H3StreamWriter<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
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

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        <Self as Sink<&[u8]>>::poll_flush(self, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        <Self as Sink<&[u8]>>::poll_close(self, cx)
    }
}

#[derive(From)]
pub enum H3RecvStreamOwner {
    Client(h3::client::RequestStream<h3_shim::RecvStream, Bytes>),
    Server(h3::server::RequestStream<h3_shim::RecvStream, Bytes>),
}

/// A wrapper around a `h3::RequestStream<RecvStream, Bytes>` that implements [`Stream`].
///
/// For [`AsyncRead`], wrapper this in [`tokio_util::io::StreamReader`].
///
/// [`AsyncRead`]: tokio::io::AsyncRead
pub struct H3StreamOwnerReader {
    bytes: VecDeque<Bytes>,
    stream: H3RecvStreamOwner,
    _send_requst: H3SendRequest,
}

impl H3StreamOwnerReader {
    pub fn new(
        stream: impl Into<H3RecvStreamOwner>,
        send_requst: H3SendRequest,
    ) -> H3StreamOwnerReader {
        H3StreamOwnerReader {
            bytes: VecDeque::new(),
            stream: stream.into(),
            _send_requst: send_requst,
        }
    }
}

impl AsyncRead for H3StreamOwnerReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let filled_len = buf.filled().len();
        loop {
            while buf.has_remaining_mut()
                && let Some(front) = this.bytes.front_mut()
            {
                let slice = front.split_to(front.remaining().min(buf.remaining()));
                buf.put_slice(&slice);
                if front.is_empty() {
                    this.bytes.pop_front();
                }
            }

            if buf.filled().len() > filled_len {
                return Poll::Ready(Ok(()));
            }

            let poll_recv_data = match &mut this.stream {
                H3RecvStreamOwner::Client(stream) => stream
                    .poll_recv_data(cx)
                    .map_ok(|buf| buf.map(|mut buf| buf.copy_to_bytes(buf.remaining()))),
                H3RecvStreamOwner::Server(stream) => stream
                    .poll_recv_data(cx)
                    .map_ok(|buf| buf.map(|mut buf| buf.copy_to_bytes(buf.remaining()))),
            };

            match ready!(poll_recv_data.map_err(io::Error::other)?) {
                Some(buf) => this.bytes.push_back(buf),
                None => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl Stream for H3StreamOwnerReader {
    type Item = io::Result<Frame<Bytes>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.get_mut().stream {
            H3RecvStreamOwner::Client(request_stream) => request_stream
                .poll_recv_data(cx)
                .map_err(io::Error::other)
                .map(|r| r.transpose())
                .map_ok(|mut buf| Frame::data(buf.copy_to_bytes(buf.remaining()))),
            H3RecvStreamOwner::Server(request_stream) => request_stream
                .poll_recv_data(cx)
                .map_err(io::Error::other)
                .map(|r| r.transpose())
                .map_ok(|mut buf| Frame::data(buf.copy_to_bytes(buf.remaining()))),
        }
    }
}

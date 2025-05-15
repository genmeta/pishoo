use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use bytes::{Buf, BufMut, Bytes};
use derive_more::From;
use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(From)]
pub enum H3RecvStream<'s> {
    Client(&'s mut h3::client::RequestStream<h3_shim::RecvStream, Bytes>),
    Server(&'s mut h3::server::RequestStream<h3_shim::RecvStream, Bytes>),
}

pub struct H3StreamReader<'s> {
    bytes: VecDeque<Bytes>,
    stream: H3RecvStream<'s>,
}

impl H3StreamReader<'_> {
    pub fn new<'s>(stream: impl Into<H3RecvStream<'s>>) -> H3StreamReader<'s> {
        H3StreamReader {
            bytes: VecDeque::new(),
            stream: stream.into(),
        }
    }
}

impl AsyncRead for H3StreamReader<'_> {
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
                H3RecvStream::Client(stream) => stream
                    .poll_recv_data(cx)
                    .map_ok(|buf| buf.map(|mut buf| buf.copy_to_bytes(buf.remaining()))),
                H3RecvStream::Server(stream) => stream
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

#[derive(From)]
pub enum H3SendStream<'s> {
    Client(&'s mut h3::client::RequestStream<h3_shim::SendStream<Bytes>, Bytes>),
    Server(&'s mut h3::server::RequestStream<h3_shim::SendStream<Bytes>, Bytes>),
}

#[allow(clippy::large_enum_variant)]
pub enum H3StreamWriter<'s> {
    Send(BoxFuture<'s, (H3SendStream<'s>, io::Result<usize>)>),
    Finish(BoxFuture<'s, (H3SendStream<'s>, io::Result<()>)>),
    Idle(H3SendStream<'s>),
    Invalid,
}

impl H3StreamWriter<'_> {
    pub fn new<'s>(stream: impl Into<H3SendStream<'s>>) -> H3StreamWriter<'s> {
        H3StreamWriter::Idle(stream.into())
    }
}

impl AsyncWrite for H3StreamWriter<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        loop {
            match this {
                H3StreamWriter::Send(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    *this = Self::Idle(stream);
                    return Poll::Ready(result);
                }
                H3StreamWriter::Finish(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    *this = Self::Idle(stream);
                    result?;
                }
                H3StreamWriter::Idle(..) => {
                    let mut h3_send_stream = match std::mem::replace(this, Self::Invalid) {
                        H3StreamWriter::Idle(stream) => stream,
                        _ => unreachable!(),
                    };
                    let buf = Bytes::copy_from_slice(buf);
                    *this = Self::Send(Box::pin(async move {
                        let result = match &mut h3_send_stream {
                            H3SendStream::Client(stream) => stream.send_data(buf.clone()).await,
                            H3SendStream::Server(stream) => stream.send_data(buf.clone()).await,
                        };
                        let result = result.map(|_| buf.len()).map_err(io::Error::other);
                        (h3_send_stream, result)
                    }))
                }
                H3StreamWriter::Invalid => {
                    tracing::error!("H3StreamWriter state error(write)");
                    unreachable!()
                }
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
            H3StreamWriter::Finish(future) => {
                let (stream, result) = ready!(future.as_mut().poll(cx));
                *this = Self::Idle(stream);
                Poll::Ready(result.map(|_| ()))
            }
            H3StreamWriter::Idle(_) => Poll::Ready(Ok(())),
            H3StreamWriter::Invalid => {
                tracing::error!("H3StreamWriter state error(flush)");
                unreachable!()
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        loop {
            match this {
                H3StreamWriter::Finish(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    *this = Self::Idle(stream);
                    return Poll::Ready(result.map(|_| ()));
                }
                H3StreamWriter::Idle(..) => {
                    let mut h3_send_stream = match std::mem::replace(this, Self::Invalid) {
                        H3StreamWriter::Idle(stream) => stream,
                        _ => unreachable!(),
                    };
                    *this = Self::Finish(Box::pin(async move {
                        let result = match &mut h3_send_stream {
                            H3SendStream::Client(stream) => stream.finish().await,
                            H3SendStream::Server(stream) => stream.finish().await,
                        };
                        (h3_send_stream, result.map_err(io::Error::other))
                    }))
                }
                H3StreamWriter::Send(..) | H3StreamWriter::Invalid => {
                    tracing::error!("H3StreamWriter state error(shutdown)");
                    unreachable!()
                }
            }
        }
    }
}

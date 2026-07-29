use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::info;

pub(super) struct ObservedIo<T> {
    inner: T,
    label: &'static str,
    read_total: u64,
    write_total: u64,
}

impl<T> ObservedIo<T> {
    pub(super) fn new(inner: T, label: &'static str) -> Self {
        Self {
            inner,
            label,
            read_total: 0,
            write_total: 0,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ObservedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        match Pin::new(&mut this.inner).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                let bytes = buffer.filled().len() - before;
                this.read_total += bytes as u64;
                info!(
                    endpoint = this.label,
                    bytes,
                    total = this.read_total,
                    "WebSocket tunnel read"
                );
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ObservedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(context, buffer) {
            Poll::Ready(Ok(bytes)) => {
                this.write_total += bytes as u64;
                info!(
                    endpoint = this.label,
                    bytes,
                    total = this.write_total,
                    "WebSocket tunnel write"
                );
                Poll::Ready(Ok(bytes))
            }
            result => result,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(context) {
            Poll::Ready(Ok(())) => {
                info!(
                    endpoint = this.label,
                    total = this.write_total,
                    "WebSocket tunnel flush"
                );
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

/// Recombine independent H3 read and write streams for Tokio's bidirectional IO APIs.
pub(super) struct TunnelIo<R, W> {
    reader: Pin<Box<R>>,
    writer: Pin<Box<W>>,
}

impl<R, W> TunnelIo<R, W> {
    pub(super) fn new(reader: R, writer: W) -> Self {
        Self {
            reader: Box::pin(reader),
            writer: Box::pin(writer),
        }
    }
}

impl<R: AsyncRead, W> AsyncRead for TunnelIo<R, W> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut().reader.as_mut().poll_read(context, buffer)
    }
}

impl<R, W: AsyncWrite> AsyncWrite for TunnelIo<R, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().writer.as_mut().poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().writer.as_mut().poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().writer.as_mut().poll_shutdown(context)
    }
}

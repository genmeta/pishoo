use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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

use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Sink;

pin_project_lite::pin_project! {
    pub struct MappedSink<S, Item, F, U, E> {
        #[pin]
        sink: S,
        mapper: F,
        _maker: PhantomData<(Item, U, E)>,
    }
}

impl<S, Item, F, U, E> MappedSink<S, Item, F, U, E> {
    pub fn new(sink: S, mapper: F) -> Self {
        Self {
            sink,
            mapper,
            _maker: PhantomData,
        }
    }
}

impl<S: Clone, Item, F: Clone, U, E> Clone for MappedSink<S, Item, F, U, E> {
    fn clone(&self) -> Self {
        Self {
            sink: self.sink.clone(),
            mapper: self.mapper.clone(),
            _maker: self._maker,
        }
    }
}

impl<S, Item, F, U, E> Sink<Item> for MappedSink<S, Item, F, U, E>
where
    S: Sink<U, Error = E>,
    F: FnMut(Item) -> Result<U, E>,
{
    type Error = E;

    #[inline]
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().sink.poll_ready(cx)
    }

    #[inline]
    fn start_send(self: Pin<&mut Self>, item: Item) -> Result<(), Self::Error> {
        let project = self.project();
        let item = (project.mapper)(item)?;
        project.sink.start_send(item)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().sink.poll_flush(cx)
    }

    #[inline]
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().sink.poll_close(cx)
    }
}

pub trait MapSinkExt<Item>: Sink<Item> {
    #[inline]
    fn mapped<F, U, E>(self, mapper: F) -> MappedSink<Self, U, F, Item, E>
    where
        Self: Sized,
        F: FnMut(U) -> Result<Item, E>,
    {
        MappedSink::new(self, mapper)
    }
}

impl<S, Item> MapSinkExt<Item> for S where S: Sink<Item> {}

use std::{
    convert::Infallible,
    task::{Context, Poll},
};

use axum::{body::Body, extract::Request, response::Response};
use bytes::Bytes;
use dhttp::h3x::dhttp::message::MessageStreamError;
use futures::future::BoxFuture;
use http_body_util::combinators::UnsyncBoxBody;

type H3Body = UnsyncBoxBody<Bytes, MessageStreamError>;

/// Tower layer that converts the h3x body type (`UnsyncBoxBody<Bytes,
/// MessageStreamError>`) into `axum::body::Body` so that inner layers can use
/// standard axum middleware patterns (`from_fn`, extractors, etc.).
#[derive(Clone, Copy)]
pub struct BodyAdapterLayer;

impl<S> tower::Layer<S> for BodyAdapterLayer {
    type Service = BodyAdapter<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BodyAdapter { inner }
    }
}

#[derive(Clone)]
pub struct BodyAdapter<S> {
    inner: S,
}

impl<S> tower_service::Service<http::Request<H3Body>> for BodyAdapter<S>
where
    S: tower_service::Service<Request, Response = Response, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<H3Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let request = http::Request::from_parts(parts, Body::new(body));
            inner.call(request).await
        })
    }
}

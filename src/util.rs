use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};

pub fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| unreachable!("Full body cannot fail: {:?}", never))
        .boxed()
}

pub fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| unreachable!("Empty body cannot fail: {:?}", never))
        .boxed()
}

use std::time::Duration;

use http_body_util::{Full, StreamBody};

use super::*;

fn body(bytes: Bytes) -> Body {
    Full::new(bytes)
        .map_err(|error| match error {})
        .boxed_unsync()
}

fn encoded(frames: &[Frame]) -> Bytes {
    let mut bytes = BytesMut::new();
    for frame in frames {
        frame.encode(&mut bytes).unwrap();
    }
    bytes.freeze()
}

include!("admission.rs");
include!("codec.rs");
include!("input.rs");

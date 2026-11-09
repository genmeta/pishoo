use std::io::Write;
use wasip2::{clocks::monotonic_clock, http::types::*};

struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let target = request.path_with_query().unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let fields = Fields::new();
        fields
            .append(
                "content-type",
                match path {
                    "/sse" => b"text/event-stream".as_slice(),
                    "/ndjson" => b"application/x-ndjson".as_slice(),
                    "/binary" => b"application/octet-stream".as_slice(),
                    _ => b"text/plain".as_slice(),
                },
            )
            .unwrap();
        let response = OutgoingResponse::new(fields);
        let body = response.body().unwrap();
        let mut stream = body.write().unwrap();
        ResponseOutparam::set(out, Ok(response));
        if stream.write_all(b"ready\n").is_err() || stream.blocking_flush().is_err() {
            return;
        }
        if path == "/trap" {
            panic!("stream probe trap after headers");
        }
        if path == "/eof" {
            drop(stream);
            let _ = OutgoingBody::finish(body, None);
            monotonic_clock::subscribe_duration(60_000_000_000).block();
            return;
        }
        let delay = if path == "/budget" {
            1_000_000_000
        } else if path == "/idle" {
            60_000_000_000
        } else {
            50_000_000
        };
        let count = query.strip_prefix("count=").and_then(|s| s.parse().ok());
        let chunk = if path == "/binary" {
            vec![0xa5; 16 * 1024]
        } else {
            b"tick\n".to_vec()
        };
        let mut index = 0u32;
        loop {
            if path != "/binary" {
                monotonic_clock::subscribe_duration(delay).block();
            }
            if path == "/budget" {
                for step in 0..1_000_000u64 {
                    std::hint::black_box(step);
                }
            }
            if stream.write_all(&chunk).is_err() || stream.blocking_flush().is_err() {
                return;
            }
            index += 1;
            if count == Some(index) {
                break;
            }
        }
        drop(stream);
        let _ = OutgoingBody::finish(body, None);
    }
}

wasip2::http::proxy::export!(Handler);

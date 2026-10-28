use std::io::Write;
use wasip2::http::types::*;
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit/pishoo-events",
        inline: "package test:events; world test { import pishoo:events/change@0.1.0; }",
        generate_all,
    });
}
use bindings::pishoo::events::change;
struct Handler;
impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let fields = Fields::new();
        fields.append("content-type", b"text/event-stream").unwrap();
        let response = OutgoingResponse::new(fields);
        let body = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        let mut stream = body.write().unwrap();
        if path == "/notify" {
            change::notify();
            drop(stream);
            OutgoingBody::finish(body, None).unwrap();
            return;
        }
        let mut generation = change::current();
        if stream.write_all(b"ready\n").is_err() || stream.blocking_flush().is_err() {
            return;
        }
        loop {
            let Some(next) = change::wait(generation, 15_000) else { break; };
            generation = next;
            if stream.write_all(b"changed\n").is_err() || stream.blocking_flush().is_err() {
                return;
            }
        }
        drop(stream);
        let _ = OutgoingBody::finish(body, None);
    }
}
wasip2::http::proxy::export!(Handler);

use std::io::Write;

use wasip2::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_else(|| "/".into());
        let headers = Fields::new();
        headers.append("content-type", b"text/plain").unwrap();
        let response = OutgoingResponse::new(headers);
        response.set_status_code(200).unwrap();
        let body = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        let mut output = body.write().unwrap();
        write!(output, "info Lib handled {path}\n").unwrap();
        drop(output);
        OutgoingBody::finish(body, None).unwrap();
    }
}

wasip2::http::proxy::export!(Handler);

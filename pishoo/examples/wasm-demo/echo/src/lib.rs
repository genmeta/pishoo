use std::io::{Read, Write};

use wasip2::http::types::{
    Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        if !matches!(request.method(), Method::Post) {
            ResponseOutparam::set(
                out,
                Err(wasip2::http::types::ErrorCode::HttpRequestMethodInvalid),
            );
            return;
        }
        let incoming = request.consume().expect("request body");
        let mut input = incoming.stream().expect("request stream");
        let headers = Fields::new();
        headers
            .append("content-type", b"application/octet-stream")
            .unwrap();
        let response = OutgoingResponse::new(headers);
        response.set_status_code(200).unwrap();
        let body = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        let mut output = body.write().unwrap();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = Read::read(&mut input, &mut chunk).expect("read request body");
            if count == 0 {
                break;
            }
            if output.write_all(&chunk[..count]).is_err() || output.flush().is_err() {
                return;
            }
        }
        drop(input);
        let _ = IncomingBody::finish(incoming);
        drop(output);
        OutgoingBody::finish(body, None).unwrap();
    }
}

wasip2::http::proxy::export!(Handler);

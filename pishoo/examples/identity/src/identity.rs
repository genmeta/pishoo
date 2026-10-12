use std::io::Write;

use pishoo_sdk::request_identity;
use wasip2::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let document = serde_json::json!({
            "local_name": request_identity::local_name(),
            "caller_name": request_identity::caller_name(),
            "caller_is_owner": request_identity::caller_is_owner(),
        });
        let bytes = serde_json::to_vec(&document).unwrap();
        let fields = Fields::new();
        fields
            .set("content-type", &[b"application/json".to_vec()])
            .unwrap();
        fields
            .set("cache-control", &[b"no-store".to_vec()])
            .unwrap();
        let response = OutgoingResponse::new(fields);
        response.set_status_code(200).unwrap();
        let body = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        if !matches!(request.method(), Method::Head) {
            body.write().unwrap().write_all(&bytes).unwrap();
        }
        OutgoingBody::finish(body, None).unwrap();
    }
}

wasip2::http::proxy::export!(Handler);

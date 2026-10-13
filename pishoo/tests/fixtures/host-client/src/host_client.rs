use std::io::{Read, Write};
use wasip2::http::{outgoing_handler, types::*};

struct Handler;

fn call(target: &str, path: &str) -> Result<Vec<u8>, String> {
    let request = OutgoingRequest::new(Fields::new());
    request.set_scheme(Some(&Scheme::Https)).unwrap();
    request.set_authority(Some(target)).unwrap();
    request.set_path_with_query(Some(path)).unwrap();
    let future = outgoing_handler::handle(request, None).map_err(|e| format!("{e:?}"))?;
    future.subscribe().block();
    let response = future
        .get()
        .unwrap()
        .unwrap()
        .map_err(|e| format!("{e:?}"))?;
    let status = response.status();
    let body = response.consume().unwrap();
    let mut bytes = Vec::new();
    body.stream().unwrap().read_to_end(&mut bytes).unwrap();
    if status != 200 {
        return Err(format!("status {status}"));
    }
    Ok(bytes)
}

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let document = if request
            .path_with_query()
            .unwrap_or_default()
            .starts_with("/identity")
        {
            serde_json::json!({"local": pishoo_sdk::identity::local(), "caller": pishoo_sdk::identity::caller()})
        } else {
            match call(
                &request
                    .headers()
                    .get("x-target")
                    .first()
                    .map(|bytes| String::from_utf8(bytes.clone()).unwrap())
                    .unwrap_or_else(|| "~".into()),
                "/std/native",
            ) {
                Ok(bytes) => serde_json::json!({"ok": String::from_utf8(bytes).unwrap()}),
                Err(error) => serde_json::json!({"error": error}),
            }
        };
        let response = OutgoingResponse::new(Fields::new());
        let body = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        body.write()
            .unwrap()
            .write_all(&serde_json::to_vec(&document).unwrap())
            .unwrap();
        OutgoingBody::finish(body, None).unwrap();
    }
}
impl wasip2::exports::cli::run::Guest for Handler {
    fn run() -> Result<(), ()> {
        assert!(pishoo_sdk::identity::caller().is_none());
        assert!(!pishoo_sdk::identity::local().1.is_empty());
        let bytes = call("~", "/std/native").map_err(|_| ())?;
        std::fs::write("/db/run-result", bytes).map_err(|_| ())
    }
}
wasip2::http::proxy::export!(Handler);
wasip2::cli::command::export!(Handler);

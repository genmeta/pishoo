use std::io::{Read,Write};
use wasip2::http::{types::*,outgoing_handler};
struct Probe;
fn fetch()->Result<serde_json::Value,()> {
    let caller=pishoo_sdk::identity::caller();
    let req=OutgoingRequest::new(Fields::new());
    req.set_method(&Method::Get).map_err(|_|())?;
    req.set_scheme(Some(&Scheme::Https)).map_err(|_|())?;
    req.set_authority(Some("~")).map_err(|_|())?;
    req.set_path_with_query(Some("/profile/profile/context")).map_err(|_|())?;
    let body=req.body().map_err(|_|())?;
    OutgoingBody::finish(body,None).map_err(|_|())?;
    let future=outgoing_handler::handle(req,None).map_err(|_|())?;
    future.subscribe().block();
    let res=future.get().ok_or(())?.map_err(|_|())?.map_err(|_|())?;
    let status=res.status();
    let body=res.consume().map_err(|_|())?;
    let mut bytes=Vec::new();
    body.stream().map_err(|_|())?.take(64*1024).read_to_end(&mut bytes).map_err(|_|())?;
    let value=serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_else(|_|serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Ok(serde_json::json!({"caller":caller,"status":status,"response":value}))
}
impl wasip2::exports::http::incoming_handler::Guest for Probe {
    fn handle(_:IncomingRequest,out:ResponseOutparam) {
        let value=fetch().unwrap_or_else(|_|serde_json::json!({"error":"local HTTP failed"}));
        let fields=Fields::new();fields.append("content-type",b"application/json").unwrap();
        let response=OutgoingResponse::new(fields);let body=response.body().unwrap();
        ResponseOutparam::set(out,Ok(response));
        if let Ok(mut stream)=body.write() {stream.write_all(&serde_json::to_vec(&value).unwrap()).unwrap();}
        OutgoingBody::finish(body,None).unwrap();
    }
}
impl wasip2::exports::cli::run::Guest for Probe {
    fn run()->Result<(),()> {std::fs::write("/db/response.json",serde_json::to_vec(&fetch()?).unwrap()).map_err(|_|())}
}
wasip2::http::proxy::export!(Probe);
wasip2::cli::command::export!(Probe);

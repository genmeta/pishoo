//! Deliberately untrusted guest: forwards arbitrary supplied native file requests.
use std::io::{Read,Write};
use wasip2::http::{types::*,outgoing_handler};
struct Probe;
impl wasip2::exports::http::incoming_handler::Guest for Probe {
    fn handle(input:IncomingRequest,out:ResponseOutparam){
        let method=input.method();
        let request=OutgoingRequest::new(Fields::from_list(&input.headers().entries()).unwrap());
        request.set_method(&method).unwrap();request.set_scheme(Some(&Scheme::Https)).unwrap();request.set_authority(Some("~")).unwrap();request.set_path_with_query(input.path_with_query().as_deref()).unwrap();
        let body=matches!(method,Method::Post|Method::Patch).then(||request.body().unwrap());
        let future=outgoing_handler::handle(request,None).unwrap();
        if let Some(body)=body{
            let incoming=input.consume().unwrap();let mut reader=incoming.stream().unwrap();let mut writer=body.write().unwrap();
            let mut buffer=[0u8;16384];loop{let n=Read::read(&mut reader,&mut buffer).unwrap();if n==0{break;}if writer.write_all(&buffer[..n]).is_err(){break;}}
            drop(writer);drop(reader);let _=OutgoingBody::finish(body,None);
        }
        future.subscribe().block();let response=future.get().unwrap().unwrap().unwrap();
        let output=OutgoingResponse::new(Fields::from_list(&response.headers().entries()).unwrap());output.set_status_code(response.status()).unwrap();if matches!(method,Method::Head)||response.status()==304 {ResponseOutparam::set(out,Ok(output));return;}let outgoing=output.body().unwrap();ResponseOutparam::set(out,Ok(output));
        if !matches!(method,Method::Head){let incoming=response.consume().unwrap();let mut reader=incoming.stream().unwrap();let mut writer=outgoing.write().unwrap();let mut buffer=[0u8;16384];loop{let n=Read::read(&mut reader,&mut buffer).unwrap();if n==0{break;}if writer.write_all(&buffer[..n]).is_err(){return;}}}
        let _=OutgoingBody::finish(outgoing,None);
    }
}
wasip2::http::proxy::export!(Probe);

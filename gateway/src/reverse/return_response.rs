use axum::{Extension, body::Body};
use http::{Response, header};

use crate::{command, parse::types::ReturnResponse, reverse::location::LocationMatch};

pub async fn return_handle(Extension(loc): Extension<LocationMatch>) -> axum::response::Response {
    let location = &loc.location;
    let response = build_response(
        location
            .return_response()
            .expect("return handler requires return directive"),
    );
    let (mut parts, body) = response.into_parts();
    command::add_header(location, &mut parts);
    Response::from_parts(parts, body)
}

fn build_response(directive: &ReturnResponse) -> axum::response::Response {
    match directive {
        ReturnResponse::Status(status) => Response::builder()
            .status(*status)
            .body(Body::empty())
            .expect("valid return response"),
        ReturnResponse::Text { status, body } => Response::builder()
            .status(*status)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(body.clone()))
            .expect("valid return response"),
        ReturnResponse::Redirect { status, location } => Response::builder()
            .status(*status)
            .header(header::LOCATION, location.clone())
            .body(Body::empty())
            .expect("valid return redirect response"),
    }
}

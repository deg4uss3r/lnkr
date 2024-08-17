use fastly::http::{header, Method, StatusCode};
use fastly::{mime, Error, Request, Response};

#[fastly::main]
fn main(req: Request) -> Result<Response, Error> {
    // Log service version
    println!(
        "FASTLY_SERVICE_VERSION: {}",
        std::env::var("FASTLY_SERVICE_VERSION").unwrap_or_else(|_| String::new())
    );

    // Filter request methods...
    match req.get_method() {
        // Right now only allow GET requests
        &Method::GET => (),
        // Block all requests with unexpected methods
        _ => {
            return Ok(Response::from_status(StatusCode::METHOD_NOT_ALLOWED)
                .with_header(header::ALLOW, "GET")
                .with_body_text_plain("This method is not allowed\n"))
        }
    };

    // Pattern match on the path...
    match req.get_path() {
        "/" => {
            // Send a default synthetic response.
            Ok(Response::from_status(StatusCode::OK)
                .with_content_type(mime::TEXT_HTML_UTF_8)
                .with_body(include_str!("index.html")))
        }

        // Catch all other requests and return a 404.
        _ => Ok(Response::from_status(StatusCode::NOT_FOUND)
            .with_body_text_plain("The page you requested could not be found\n")),
    }
}

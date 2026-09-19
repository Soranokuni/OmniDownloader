use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
pub struct WebAssets;

pub struct EmbeddedFile(pub &'static str);

impl IntoResponse for EmbeddedFile {
    fn into_response(self) -> Response {
        match WebAssets::get(self.0) {
            Some(content) => {
                let mime = mime_guess::from_path(self.0).first_or_octet_stream();
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, HeaderValue::from_str(mime.as_ref()).unwrap()),
                        (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
                    ],
                    content.data,
                )
                    .into_response()
            }
            None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
        }
    }
}

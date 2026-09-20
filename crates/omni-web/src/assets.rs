use axum::extract::Path as AxumPath;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
pub struct WebAssets;

/// An HTML page. Never cached: a panel served from cache after an upgrade shows
/// an operator a UI that no longer matches the API behind it.
pub struct EmbeddedFile(pub &'static str);

impl IntoResponse for EmbeddedFile {
    fn into_response(self) -> Response {
        match WebAssets::get(self.0) {
            Some(content) => {
                let mime = mime_guess::from_path(self.0).first_or_octet_stream();
                (
                    StatusCode::OK,
                    [
                        (
                            header::CONTENT_TYPE,
                            HeaderValue::from_str(mime.as_ref()).unwrap(),
                        ),
                        (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                    ],
                    content.data,
                )
                    .into_response()
            }
            None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
        }
    }
}

/// Serve `assets/static/*` — the stylesheet, the shared script and the icon
/// sprite that replaced the CDN tags (plan P2.5).
///
/// Public, because they carry no data, and cached for a day: the panels append
/// a build-stamped query string, so a new build is a new URL and the cache
/// never serves stale code.
pub async fn serve_static(AxumPath(file): AxumPath<String>) -> Response {
    // `axum`'s `*file` capture cannot contain a leading `/`, but it can contain
    // `..` segments. Everything is served out of a compiled-in table rather
    // than the filesystem, so a traversal cannot escape anywhere — but reject
    // it anyway so the rule does not depend on `rust-embed`'s lookup semantics.
    if file.contains("..") || file.contains('\\') {
        return (StatusCode::NOT_FOUND, "404 Not Found").into_response();
    }

    let path = format!("static/{}", file);
    match WebAssets::get(&path) {
        Some(content) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_str(mime.as_ref()).unwrap(),
                    ),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("public, max-age=86400"),
                    ),
                ],
                content.data,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
    }
}

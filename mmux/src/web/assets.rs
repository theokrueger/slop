use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// Frontend assets are embedded in release builds and read from `web/` in
/// debug builds by rust-embed.
#[derive(RustEmbed)]
#[folder = "web/"]
pub struct Assets;

pub fn serve(path: &str) -> Response {
    if path.is_empty() || path.contains("..") || path.starts_with('/') {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    let Some(asset) = Assets::get(path) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut response = ([(header::CONTENT_TYPE, mime.as_ref())], asset.data).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

pub fn html_utf8(name: &str) -> Response {
    match Assets::get(name) {
        Some(asset) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            asset.data,
        )
            .into_response(),
        None => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

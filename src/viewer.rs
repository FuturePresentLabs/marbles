//! Read-only browser shell for the `views.*` APIs.
//!
//! The assets are embedded in the binary so a hosted deployment does not need a second static
//! server. `/view` and its assets are public; every `/v1` route still authenticates normally.

use axum::http::header;
use axum::response::{Html, IntoResponse, Response};

const INDEX: &str = include_str!("../viewer/index.html");
const APP_JS: &str = include_str!("../viewer/app.js");
const STYLE_CSS: &str = include_str!("../viewer/style.css");

pub async fn index() -> Html<&'static str> {
    Html(INDEX)
}

pub async fn app_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
        .into_response()
}

pub async fn style_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}

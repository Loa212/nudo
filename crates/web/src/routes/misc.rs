use super::*;

// ---------------------------------------------------------------------------
// Assets and errors
// ---------------------------------------------------------------------------

pub async fn asset(Path(name): Path<String>) -> Response {
    crate::assets::serve(&name).await
}

pub async fn not_found() -> Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        Html(render::error_page(404, "That page does not exist.").into_string()),
    )
        .into_response()
}

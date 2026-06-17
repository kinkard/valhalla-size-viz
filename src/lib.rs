use axum::{
    Router,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use tokio::{fs::File, io::AsyncReadExt};

pub mod api;
pub mod cache;
pub mod tiles;
pub mod upstream;

pub use api::{AppState, tile_sizes};
pub use cache::SizeCache;
pub use upstream::RatiClient;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(serve_index_html))
        .route("/api/tile-sizes", post(tile_sizes))
        .route("/healthz", get(healthz))
        .with_state(state)
}

pub async fn serve_index_html() -> Result<Html<String>, (StatusCode, String)> {
    let index_html = "web/index.html";
    let Ok(mut file) = File::open(index_html).await else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Failed to open {index_html}: not found"),
        ));
    };

    let mut contents = String::new();
    if let Err(err) = file.read_to_string(&mut contents).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read {index_html}: {err}"),
        ));
    }

    Ok(Html(contents))
}

pub async fn healthz() -> &'static str {
    "OK"
}

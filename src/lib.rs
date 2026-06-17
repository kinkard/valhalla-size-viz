use std::path::{Path, PathBuf};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use tokio::{fs::File, io::AsyncReadExt};
use tower_http::services::ServeDir;

pub mod api;
pub mod cache;
pub mod tiles;
pub mod upstream;

pub use api::{AppState, tile_sizes};
pub use cache::SizeCache;
pub use upstream::RatiClient;

/// Where the bundled HTML/JS lives on disk. Same directory used by `ServeDir`
/// for `countries.js`, `poly-data.js`, and the `poly/` GeoJSON tree.
pub const WEB_DIR: &str = "web";

/// Default upper bound for JSON request bodies. 4 MiB comfortably fits a
/// MAX_BATCH_SIZE batch (~25 bytes per tile) with headroom.
pub const REQUEST_BODY_LIMIT: usize = 4 * 1024 * 1024;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route(
            "/api/tile-sizes",
            post(tile_sizes).layer(DefaultBodyLimit::max(REQUEST_BODY_LIMIT)),
        )
        .route("/healthz", get(healthz))
        // Fallback for `countries.js`, `poly-data.js`, and the `poly/` GeoJSON tree.
        .fallback_service(ServeDir::new(WEB_DIR))
        .with_state(state)
}

async fn serve_index() -> Result<Html<String>, (StatusCode, String)> {
    serve_index_html(Path::new(WEB_DIR).join("index.html")).await
}

/// Read an `index.html` file from disk and return its body. Exposed for tests so
/// we can drive both the success path and the 404/IO-error branches without
/// touching the bundled `web/` directory.
pub async fn serve_index_html(
    path: impl Into<PathBuf>,
) -> Result<Html<String>, (StatusCode, String)> {
    let path = path.into();
    let mut file = match File::open(&path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("{}: not found", path.display()),
            ));
        }
        Err(err) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to open {}: {err}", path.display()),
            ));
        }
    };

    let mut contents = String::new();
    if let Err(err) = file.read_to_string(&mut contents).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read {}: {err}", path.display()),
        ));
    }

    Ok(Html(contents))
}

pub async fn healthz() -> &'static str {
    "OK"
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[tokio::test]
    async fn serve_index_html_returns_not_found_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.html");
        let result = serve_index_html(path).await;
        let (status, _msg) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_index_html_reads_file_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.html");
        tokio::fs::write(&path, "<html><body>hello</body></html>")
            .await
            .unwrap();
        let Html(body) = serve_index_html(path).await.unwrap();
        assert_eq!(body, "<html><body>hello</body></html>");
    }
}

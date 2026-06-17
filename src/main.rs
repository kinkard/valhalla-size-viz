use std::{
    num::NonZeroU16,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use clap::Parser;
use rustc_hash::FxBuildHasher;
use tokio::{fs::File, io::AsyncReadExt, signal, sync::Semaphore};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::info;

use crate::api::{AppState, SizeCache, tile_sizes};
use crate::upstream::RatiClient;

mod api;
mod tiles;
mod upstream;

/// Where the bundled HTML/JS lives on disk. Same directory used by `ServeDir`
/// for `countries.js`, `poly-data.js`, and the `poly/` GeoJSON tree.
const WEB_DIR: &str = "web";

/// Default upper bound for JSON request bodies. 4 MiB comfortably fits a
/// MAX_BATCH_SIZE batch (~25 bytes per tile) with headroom.
const REQUEST_BODY_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Config {
    /// Port to listen on
    #[arg(long, default_value_t = 3000)]
    port: u16,
    /// Max concurrent upstream fetches to rati (1..=65535)
    #[arg(long, default_value_t = NonZeroU16::new(32).unwrap())]
    concurrency: NonZeroU16,
    /// rati base URL (e.g. http://localhost:8050)
    #[arg(long)]
    rati_url: String,
}

fn main() {
    tracing_subscriber::fmt::init();

    let config = Config::parse();

    // 4 worker threads is plenty for an I/O-bound proxy — async tasks share
    // threads, so we don't need one per in-flight upstream request. The
    // `--concurrency` flag bounds upstream fan-out via a Semaphore instead.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(run(config))
}

async fn run(config: Config) {
    let concurrency = usize::from(config.concurrency.get());
    let rati =
        Arc::new(RatiClient::new(config.rati_url.clone()).expect("failed to build reqwest client"));
    let state = AppState {
        rati,
        cache: Arc::new(SizeCache::with_hasher(FxBuildHasher)),
        upstream_permits: Arc::new(Semaphore::new(concurrency)),
    };

    let app = build_router(state).layer(TraceLayer::new_for_http());

    let bind_addr = ("0.0.0.0", config.port);
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .unwrap_or_else(|err| panic!("failed to bind to {}:{}: {err}", bind_addr.0, bind_addr.1));
    let bound = listener
        .local_addr()
        .expect("failed to read bound local address");
    info!(
        "Listening at http://localhost:{} (rati upstream: {})",
        bound.port(),
        config.rati_url
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("axum::serve failed");
}

fn build_router(state: AppState) -> Router {
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

/// Read an `index.html` file from disk and return its body.
async fn serve_index_html(path: impl Into<PathBuf>) -> Result<Html<String>, (StatusCode, String)> {
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

async fn healthz() -> &'static str {
    "OK"
}

async fn shutdown_signal() {
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Ctrl+C received, shutting down");
        }
        _ = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM signal handler")
                .recv()
                .await
        } => {
            info!("SIGTERM received, shutting down");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use pretty_assertions::assert_eq;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState {
            rati: Arc::new(RatiClient::new("http://localhost:0".to_string()).unwrap()),
            cache: Arc::new(SizeCache::with_hasher(FxBuildHasher)),
            upstream_permits: Arc::new(Semaphore::new(32)),
        }
    }

    #[test]
    fn cli_parses_all_flags() {
        let cfg = Config::try_parse_from([
            "valhalla-size-viz",
            "--rati-url",
            "http://example:8050",
            "--port",
            "4000",
            "--concurrency",
            "16",
        ])
        .unwrap();
        assert_eq!(cfg.port, 4000);
        assert_eq!(cfg.concurrency.get(), 16);
        assert_eq!(cfg.rati_url, "http://example:8050");
    }

    #[test]
    fn cli_defaults_match_documented_values() {
        let cfg =
            Config::try_parse_from(["valhalla-size-viz", "--rati-url", "http://localhost:8050"])
                .unwrap();
        assert_eq!(cfg.port, 3000);
        assert_eq!(cfg.concurrency.get(), 32);
    }

    #[test]
    fn cli_rejects_concurrency_zero() {
        // Catches the historical regression where `--concurrency 0` panicked
        // tokio's runtime builder. clap now enforces NonZeroU16.
        let err = Config::try_parse_from([
            "valhalla-size-viz",
            "--rati-url",
            "http://localhost:8050",
            "--concurrency",
            "0",
        ])
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("concurrency"), "unexpected error: {msg}");
    }

    #[test]
    fn cli_requires_rati_url() {
        assert!(Config::try_parse_from(["valhalla-size-viz"]).is_err());
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = build_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"OK");
    }
}

use std::{num::NonZero, sync::Arc};

use axum::{
    Router,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use clap::Parser;
use tokio::{fs::File, io::AsyncReadExt, signal};
use tower_http::trace::TraceLayer;
use tracing::info;

mod api;
mod cache;
mod tiles;
mod upstream;

use api::{AppState, tile_sizes};
use cache::SizeCache;
use upstream::RatiClient;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Config {
    /// Port to listen on
    #[arg(long, default_value_t = 3000)]
    port: u16,
    /// Max concurrent upstream fetches to rati
    #[arg(long, default_value_t = 32)]
    concurrency: u16,
    /// rati base URL (e.g. http://localhost:8050)
    #[arg(long, env = "RATI_URL")]
    rati_url: String,
}

fn main() {
    tracing_subscriber::fmt::init();

    let config = Config::parse();

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(
            std::thread::available_parallelism()
                .map(NonZero::get)
                .unwrap_or(16)
                .min(config.concurrency as usize),
        )
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(config))
}

async fn run(config: Config) {
    let rati =
        Arc::new(RatiClient::new(config.rati_url.clone()).expect("failed to build reqwest client"));
    let state = AppState {
        rati,
        cache: Arc::new(SizeCache::new()),
        concurrency: config.concurrency as usize,
    };

    let app = Router::new()
        .route("/", get(serve_index_html))
        .route("/api/tile-sizes", post(tile_sizes))
        .route("/healthz", get(healthz))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port))
        .await
        .unwrap();
    let bound = listener.local_addr().unwrap();
    info!(
        "Listening at http://localhost:{} (rati upstream: {})",
        bound.port(),
        config.rati_url
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
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

async fn serve_index_html() -> Result<Html<String>, (StatusCode, String)> {
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

async fn healthz() -> &'static str {
    "OK"
}

#[cfg(test)]
fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(serve_index_html))
        .route("/api/tile-sizes", post(tile_sizes))
        .route("/healthz", get(healthz))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use pretty_assertions::assert_eq;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState {
            rati: Arc::new(RatiClient::new("http://localhost:0".to_string()).unwrap()),
            cache: Arc::new(SizeCache::new()),
            concurrency: 32,
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
        assert_eq!(cfg.concurrency, 16);
        assert_eq!(cfg.rati_url, "http://example:8050");
    }

    #[test]
    fn cli_defaults_match_documented_values() {
        let cfg =
            Config::try_parse_from(["valhalla-size-viz", "--rati-url", "http://localhost:8050"])
                .unwrap();
        assert_eq!(cfg.port, 3000);
        assert_eq!(cfg.concurrency, 32);
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

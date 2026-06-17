use std::{num::NonZeroU16, sync::Arc};

use clap::Parser;
use rustc_hash::FxBuildHasher;
use tokio::{signal, sync::Semaphore};
use tower_http::trace::TraceLayer;
use tracing::info;

use valhalla_size_viz::{AppState, RatiClient, SizeCache, build_router};

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

    // The upstream concurrency knob bounds I/O fan-out, not CPU parallelism;
    // let tokio pick a reasonable worker-thread count on its own.
    tokio::runtime::Builder::new_multi_thread()
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
        concurrency,
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

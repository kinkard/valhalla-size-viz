use std::{
    num::NonZeroU16,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use dashmap::DashMap;
use futures_util::StreamExt;
use reqwest::header;
use rustc_hash::FxBuildHasher;
use serde::{Deserialize, Serialize};
use tokio::{fs::File, io::AsyncReadExt, signal};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{error, info, warn};

/// Upstream request timeout. Without this, a slow or hung rati would pin
/// `buffer_unordered` slots indefinitely and stall every concurrent batch.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the bundled HTML/JS lives on disk. Also used by `ServeDir` for
/// `countries.js`, `poly-data.js`, and the `poly/` GeoJSON tree.
const WEB_DIR: &str = "web";

/// 4 MiB comfortably fits MAX_BATCH_SIZE tiles (~25 bytes each) with headroom.
const REQUEST_BODY_LIMIT: usize = 4 * 1024 * 1024;

/// Soft cap on tiles per batch — fits inside REQUEST_BODY_LIMIT and prevents
/// accidental runaway fetches. Country-mode level-2 selections are in the low
/// thousands, so 20k is comfortable.
const MAX_BATCH_SIZE: usize = 20_000;

/// Valhalla graph tile grids: level 0 = highway (4° tiles, 90×45),
/// level 1 = arterial (1°, 360×180), level 2 = local (0.25°, 1440×720).
/// Tile IDs are row-major (id = row*cols + col); max_tile_id = cols*rows - 1.
const MAX_TILE_IDS: [u32; 3] = [90 * 45 - 1, 360 * 180 - 1, 1440 * 720 - 1];

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
    // `--concurrency` flag bounds upstream fan-out via `buffer_unordered`.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(run(config))
}

async fn run(config: Config) {
    let concurrency = usize::from(config.concurrency.get());
    let http = reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .timeout(UPSTREAM_TIMEOUT)
        .build()
        .expect("failed to build reqwest client");
    let state = AppState {
        http,
        base_url: Arc::from(config.rati_url.as_str()),
        cache: Arc::new(SizeCache::with_hasher(FxBuildHasher)),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Encoding {
    Identity,
    Gzip,
    Zstd,
}

impl Encoding {
    fn as_header_value(self) -> &'static str {
        match self {
            Encoding::Identity => "identity",
            Encoding::Gzip => "gzip",
            Encoding::Zstd => "zstd",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileId {
    level: u8,
    id: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum TileIdError {
    #[error("invalid level {level}, expected 0, 1, or 2")]
    InvalidLevel { level: u8 },
    #[error("tile id {id} out of range for level {level} (max {max})")]
    IdOutOfRange { level: u8, id: u32, max: u32 },
}

impl TileId {
    fn to_path(self) -> String {
        let dir = self.id / 1000;
        let file = self.id % 1000;
        format!("{}/{:03}/{:03}.gph", self.level, dir, file)
    }

    fn validate(self) -> Result<(), TileIdError> {
        let max = MAX_TILE_IDS
            .get(self.level as usize)
            .ok_or(TileIdError::InvalidLevel { level: self.level })?;
        if self.id > *max {
            return Err(TileIdError::IdOutOfRange {
                level: self.level,
                id: self.id,
                max: *max,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    level: u8,
    tile_id: u32,
    encoding: Encoding,
}

/// In-memory tile-size cache. `Some(bytes)` = known size, `None` = confirmed 404.
type SizeCache = DashMap<CacheKey, Option<u64>, FxBuildHasher>;

#[derive(Clone)]
struct AppState {
    http: reqwest::Client,
    base_url: Arc<str>,
    cache: Arc<SizeCache>,
    /// Per-request upstream fan-out. Not a global cap — multiple concurrent
    /// batches each fan out this wide. Fine for a single-user viz tool.
    concurrency: usize,
}

#[derive(Debug, Deserialize)]
struct TileRef {
    level: u8,
    id: u32,
}

#[derive(Debug, Deserialize)]
struct TileSizesRequest {
    encoding: Encoding,
    tiles: Vec<TileRef>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct TileSize {
    level: u8,
    id: u32,
    bytes: Option<u64>,
    cached: bool,
    missing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct TileSizesResponse {
    encoding: &'static str,
    sizes: Vec<TileSize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchOutcome {
    Found(u64),
    Missing,
}

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("upstream returned status {status} for {path}")]
    Upstream { status: u16, path: String },
    #[error("upstream returned 200 OK without Content-Length for {path}")]
    MissingContentLength { path: String },
}

async fn fetch_tile_size(
    http: &reqwest::Client,
    base_url: &str,
    tile: TileId,
    encoding: Encoding,
) -> Result<FetchOutcome, FetchError> {
    let path = tile.to_path();
    let url = format!("{}/tiles/{}", base_url.trim_end_matches('/'), path);

    // Assumption: rati's archive holds raw `.gph` tiles. For identity that
    // matches on-disk encoding, so HEAD returns Content-Length without
    // transferring the body. For gzip/zstd rati must transcode on the fly —
    // it doesn't set Content-Length on HEAD, so we fall back to GET.
    let method = if encoding == Encoding::Identity {
        reqwest::Method::HEAD
    } else {
        reqwest::Method::GET
    };

    let response = http
        .request(method, &url)
        .header(header::ACCEPT_ENCODING, encoding.as_header_value())
        .send()
        .await?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(FetchOutcome::Missing);
    }
    if !status.is_success() {
        return Err(FetchError::Upstream {
            status: status.as_u16(),
            path,
        });
    }

    if let Some(got) = response.headers().get(header::CONTENT_ENCODING) {
        let got_str = got.to_str().unwrap_or("<non-ascii>");
        if got_str != encoding.as_header_value() {
            warn!(
                tile = %path,
                requested = encoding.as_header_value(),
                got = got_str,
                "encoding mismatch"
            );
        }
    } else if !matches!(encoding, Encoding::Identity) {
        warn!(
            tile = %path,
            requested = encoding.as_header_value(),
            got = "identity",
            "encoding mismatch"
        );
    }

    // Read Content-Length straight from the header — `Response::content_length()`
    // returns None for HEAD responses (no body) even when the header is set.
    response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(FetchOutcome::Found)
        .ok_or(FetchError::MissingContentLength { path })
}

async fn tile_sizes(
    State(state): State<AppState>,
    Json(req): Json<TileSizesRequest>,
) -> Result<Response, (StatusCode, String)> {
    if req.tiles.len() > MAX_BATCH_SIZE {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "batch size {} exceeds maximum {}",
                req.tiles.len(),
                MAX_BATCH_SIZE
            ),
        ));
    }

    let encoding = req.encoding;
    let n = req.tiles.len();
    let mut results: Vec<Option<TileSize>> = (0..n).map(|_| None).collect();
    let mut misses: Vec<(usize, TileId)> = Vec::new();

    for (idx, tref) in req.tiles.iter().enumerate() {
        let tile = TileId {
            level: tref.level,
            id: tref.id,
        };
        if let Err(e) = tile.validate() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid tile at index {idx}: {e}"),
            ));
        }
        let key = CacheKey {
            level: tile.level,
            tile_id: tile.id,
            encoding,
        };
        if let Some(cached) = state.cache.get(&key).map(|r| *r.value()) {
            results[idx] = Some(TileSize {
                level: tile.level,
                id: tile.id,
                bytes: cached,
                cached: true,
                missing: cached.is_none(),
                error: None,
            });
        } else {
            misses.push((idx, tile));
        }
    }

    let http = state.http.clone();
    let base_url = state.base_url.clone();
    let cache = state.cache.clone();

    let fetched: Vec<(usize, TileId, Result<FetchOutcome, String>)> =
        futures_util::stream::iter(misses.into_iter().map(|(idx, tile)| {
            let http = http.clone();
            let base_url = base_url.clone();
            async move {
                let outcome = fetch_tile_size(&http, &base_url, tile, encoding)
                    .await
                    .map_err(|e| e.to_string());
                (idx, tile, outcome)
            }
        }))
        .buffer_unordered(state.concurrency)
        .collect()
        .await;

    for (idx, tile, outcome) in fetched {
        let key = CacheKey {
            level: tile.level,
            tile_id: tile.id,
            encoding,
        };
        let tile_size = match outcome {
            Ok(FetchOutcome::Found(b)) => {
                cache.insert(key, Some(b));
                TileSize {
                    level: tile.level,
                    id: tile.id,
                    bytes: Some(b),
                    cached: false,
                    missing: false,
                    error: None,
                }
            }
            Ok(FetchOutcome::Missing) => {
                cache.insert(key, None);
                TileSize {
                    level: tile.level,
                    id: tile.id,
                    bytes: None,
                    cached: false,
                    missing: true,
                    error: None,
                }
            }
            Err(err) => {
                // Don't poison the cache on transient upstream failures;
                // surface the error per tile so the rest of the batch lands.
                TileSize {
                    level: tile.level,
                    id: tile.id,
                    bytes: None,
                    cached: false,
                    missing: false,
                    error: Some(err),
                }
            }
        };
        results[idx] = Some(tile_size);
    }

    let sizes: Vec<TileSize> = results.into_iter().map(|r| r.expect("filled")).collect();

    // If every tile in a non-empty batch errored, surface as 502 so monitoring
    // catches a full upstream outage. Partial failures still return 200 — the
    // per-tile `error` field lets the frontend report them inline.
    let all_errored = !sizes.is_empty() && sizes.iter().all(|s| s.error.is_some());
    let status = if all_errored {
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::OK
    };
    let body = Json(TileSizesResponse {
        encoding: encoding.as_header_value(),
        sizes,
    });
    Ok((status, body).into_response())
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route(
            "/api/tile-sizes",
            post(tile_sizes).layer(DefaultBodyLimit::max(REQUEST_BODY_LIMIT)),
        )
        .route("/health", get(health))
        // Fallback covers countries.js, poly-data.js, and the poly/ tree.
        .fallback_service(ServeDir::new(WEB_DIR))
        .with_state(state)
}

async fn serve_index() -> Result<Html<String>, (StatusCode, String)> {
    serve_index_html(Path::new(WEB_DIR).join("index.html")).await
}

async fn serve_index_html(path: impl Into<PathBuf>) -> Result<Html<String>, (StatusCode, String)> {
    let path = path.into();
    let mut file = match File::open(&path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            error!(path = %path.display(), "index.html not found");
            return Err((StatusCode::NOT_FOUND, "not found".to_string()));
        }
        Err(err) => {
            error!(path = %path.display(), %err, "failed to open index.html");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            ));
        }
    };

    let mut contents = String::new();
    if let Err(err) = file.read_to_string(&mut contents).await {
        error!(path = %path.display(), %err, "failed to read index.html");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        ));
    }

    Ok(Html(contents))
}

async fn health() -> &'static str {
    "OK"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tower::ServiceExt;
    use wiremock::matchers::{header as match_header, method, path as match_path};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    fn test_state(base_url: &str, concurrency: usize) -> AppState {
        AppState {
            http: reqwest::Client::builder()
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .timeout(UPSTREAM_TIMEOUT)
                .build()
                .unwrap(),
            base_url: Arc::from(base_url),
            cache: Arc::new(SizeCache::with_hasher(FxBuildHasher)),
            concurrency,
        }
    }

    async fn call(router: Router, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/api/tile-sizes")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, v)
    }

    // ---- Encoding / TileId ----

    #[test]
    fn encoding_header_values() {
        assert_eq!(Encoding::Identity.as_header_value(), "identity");
        assert_eq!(Encoding::Gzip.as_header_value(), "gzip");
        assert_eq!(Encoding::Zstd.as_header_value(), "zstd");
    }

    #[test]
    fn encoding_deserialize_accepts_known_lowercase() {
        assert_eq!(
            serde_json::from_value::<Encoding>(json!("identity")).unwrap(),
            Encoding::Identity
        );
        assert_eq!(
            serde_json::from_value::<Encoding>(json!("gzip")).unwrap(),
            Encoding::Gzip
        );
        assert_eq!(
            serde_json::from_value::<Encoding>(json!("zstd")).unwrap(),
            Encoding::Zstd
        );
    }

    #[test]
    fn encoding_deserialize_rejects_unknown_and_uppercase() {
        assert!(serde_json::from_value::<Encoding>(json!("br")).is_err());
        assert!(serde_json::from_value::<Encoding>(json!("Gzip")).is_err());
        assert!(serde_json::from_value::<Encoding>(json!("ZSTD")).is_err());
        assert!(serde_json::from_value::<Encoding>(json!("")).is_err());
    }

    #[test]
    fn to_path_matches_js_reference() {
        assert_eq!(
            TileId {
                level: 2,
                id: 818660
            }
            .to_path(),
            "2/818/660.gph"
        );
        assert_eq!(TileId { level: 0, id: 529 }.to_path(), "0/000/529.gph");
        assert_eq!(TileId { level: 1, id: 0 }.to_path(), "1/000/000.gph");
        assert_eq!(
            TileId {
                level: 2,
                id: 1_000
            }
            .to_path(),
            "2/001/000.gph"
        );
    }

    #[test]
    fn validate_rejects_bad_level() {
        let err = TileId { level: 3, id: 0 }.validate().unwrap_err();
        assert_eq!(err, TileIdError::InvalidLevel { level: 3 });
        let err = TileId { level: 99, id: 0 }.validate().unwrap_err();
        assert_eq!(err, TileIdError::InvalidLevel { level: 99 });
    }

    #[test]
    fn validate_rejects_id_out_of_range() {
        let err = TileId {
            level: 0,
            id: 4_050,
        }
        .validate()
        .unwrap_err();
        assert_eq!(
            err,
            TileIdError::IdOutOfRange {
                level: 0,
                id: 4_050,
                max: 4_049
            }
        );
        let err = TileId {
            level: 2,
            id: 1_036_800,
        }
        .validate()
        .unwrap_err();
        assert_eq!(
            err,
            TileIdError::IdOutOfRange {
                level: 2,
                id: 1_036_800,
                max: 1_036_799
            }
        );
    }

    #[test]
    fn validate_accepts_boundary_values() {
        assert!(TileId { level: 0, id: 0 }.validate().is_ok());
        assert!(
            TileId {
                level: 0,
                id: 4_049
            }
            .validate()
            .is_ok()
        );
        assert!(
            TileId {
                level: 1,
                id: 64_799
            }
            .validate()
            .is_ok()
        );
        assert!(
            TileId {
                level: 2,
                id: 1_036_799
            }
            .validate()
            .is_ok()
        );
    }

    // ---- fetch_tile_size ----

    fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            .unwrap()
    }

    fn tile_2_818_660() -> TileId {
        TileId {
            level: 2,
            id: 818660,
        }
    }

    #[tokio::test]
    async fn fetch_200_returns_found_with_byte_count() {
        let server = MockServer::start().await;
        let body = vec![0xABu8; 1234];
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .and(match_header("accept-encoding", "zstd"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "zstd")
                    .set_body_bytes(body),
            )
            .mount(&server)
            .await;

        let outcome = fetch_tile_size(
            &http_client(),
            &server.uri(),
            tile_2_818_660(),
            Encoding::Zstd,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FetchOutcome::Found(1234));
    }

    #[tokio::test]
    async fn fetch_404_returns_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let outcome = fetch_tile_size(
            &http_client(),
            &server.uri(),
            tile_2_818_660(),
            Encoding::Gzip,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FetchOutcome::Missing);
    }

    #[tokio::test]
    async fn fetch_500_returns_upstream_error_with_path() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(match_path("/tiles/0/000/529.gph"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = fetch_tile_size(
            &http_client(),
            &server.uri(),
            TileId { level: 0, id: 529 },
            Encoding::Identity,
        )
        .await
        .unwrap_err();
        match err {
            FetchError::Upstream { status, path } => {
                assert_eq!(status, 500);
                assert_eq!(path, "0/000/529.gph");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_encoding_mismatch_still_counts_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .and(match_header("accept-encoding", "zstd"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(vec![0u8; 777]),
            )
            .mount(&server)
            .await;

        let outcome = fetch_tile_size(
            &http_client(),
            &server.uri(),
            tile_2_818_660(),
            Encoding::Zstd,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FetchOutcome::Found(777));
    }

    #[tokio::test]
    async fn fetch_identity_with_no_content_encoding_header_is_not_a_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(match_path("/tiles/0/000/529.gph"))
            .and(match_header("accept-encoding", "identity"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 42]))
            .mount(&server)
            .await;

        let outcome = fetch_tile_size(
            &http_client(),
            &server.uri(),
            TileId { level: 0, id: 529 },
            Encoding::Identity,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FetchOutcome::Found(42));
    }

    #[tokio::test]
    async fn base_url_trailing_slash_is_tolerated() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(match_path("/tiles/0/000/529.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 3]))
            .mount(&server)
            .await;

        let outcome = fetch_tile_size(
            &http_client(),
            &format!("{}/", server.uri()),
            TileId { level: 0, id: 529 },
            Encoding::Identity,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FetchOutcome::Found(3));
    }

    #[tokio::test]
    async fn fetch_200_without_content_length_returns_error() {
        // Chunked transfer with no Content-Length — wiremock doesn't easily
        // simulate this, but we can drive it through a raw TCP listener.
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let mut total = 0;
            loop {
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf[total..])
                    .await
                    .unwrap();
                if n == 0 {
                    break;
                }
                total += n;
                if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Encoding: gzip\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
            sock.write_all(response).await.unwrap();
            sock.flush().await.unwrap();
        });

        let err = fetch_tile_size(
            &http_client(),
            &format!("http://{addr}"),
            TileId { level: 0, id: 529 },
            Encoding::Gzip,
        )
        .await
        .unwrap_err();
        match err {
            FetchError::MissingContentLength { path } => {
                assert_eq!(path, "0/000/529.gph");
            }
            other => panic!("expected MissingContentLength, got {other:?}"),
        }
    }

    // ---- tile_sizes handler ----

    fn router(state: AppState) -> Router {
        Router::new()
            .route(
                "/api/tile-sizes",
                post(tile_sizes).layer(DefaultBodyLimit::max(REQUEST_BODY_LIMIT)),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn empty_batch_returns_empty_sizes() {
        let server = MockServer::start().await;
        let state = test_state(&server.uri(), 4);
        let (status, body) = call(router(state), json!({ "encoding": "zstd", "tiles": [] })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "encoding": "zstd", "sizes": [] }));
    }

    #[tokio::test]
    async fn cache_hit_short_circuits_upstream() {
        struct CountingResponder(Arc<AtomicUsize>);
        impl Respond for CountingResponder {
            fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
                self.0.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_bytes(vec![0u8; 999])
            }
        }
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .respond_with(CountingResponder(hits.clone()))
            .mount(&server)
            .await;

        let state = test_state(&server.uri(), 4);
        state.cache.insert(
            CacheKey {
                level: 2,
                tile_id: 818660,
                encoding: Encoding::Zstd,
            },
            Some(2435),
        );

        let (status, body) = call(
            router(state),
            json!({
                "encoding": "zstd",
                "tiles": [{ "level": 2, "id": 818660 }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "encoding": "zstd",
                "sizes": [{
                    "level": 2, "id": 818660,
                    "bytes": 2435, "cached": true, "missing": false
                }]
            })
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn mixed_cached_and_uncached_preserves_order() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 2435]))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(match_path("/tiles/1/051/234.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 999]))
            .mount(&server)
            .await;

        let state = test_state(&server.uri(), 4);
        state.cache.insert(
            CacheKey {
                level: 0,
                tile_id: 529,
                encoding: Encoding::Identity,
            },
            Some(100),
        );

        let (status, body) = call(
            router(state),
            json!({
                "encoding": "identity",
                "tiles": [
                    { "level": 0, "id": 529 },
                    { "level": 2, "id": 818660 },
                    { "level": 1, "id": 51234 }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sizes = body.get("sizes").unwrap().as_array().unwrap();
        assert_eq!(sizes.len(), 3);
        assert_eq!(sizes[0]["level"], 0);
        assert_eq!(sizes[0]["id"], 529);
        assert_eq!(sizes[0]["bytes"], 100);
        assert_eq!(sizes[0]["cached"], true);
        assert_eq!(sizes[1]["level"], 2);
        assert_eq!(sizes[1]["id"], 818660);
        assert_eq!(sizes[1]["bytes"], 2435);
        assert_eq!(sizes[1]["cached"], false);
        assert_eq!(sizes[2]["level"], 1);
        assert_eq!(sizes[2]["id"], 51234);
        assert_eq!(sizes[2]["bytes"], 999);
        assert_eq!(sizes[2]["cached"], false);
    }

    #[tokio::test]
    async fn missing_tile_propagates_as_null_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let state = test_state(&server.uri(), 4);
        let cache = state.cache.clone();
        let (status, body) = call(
            router(state),
            json!({
                "encoding": "zstd",
                "tiles": [{ "level": 2, "id": 818660 }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sizes = body.get("sizes").unwrap().as_array().unwrap();
        assert_eq!(sizes[0]["bytes"], Value::Null);
        assert_eq!(sizes[0]["missing"], true);
        assert_eq!(sizes[0]["cached"], false);

        assert_eq!(
            cache
                .get(&CacheKey {
                    level: 2,
                    tile_id: 818660,
                    encoding: Encoding::Zstd,
                })
                .map(|v| *v.value()),
            Some(None)
        );
    }

    #[tokio::test]
    async fn oversized_batch_rejected_with_400() {
        let server = MockServer::start().await;
        let state = test_state(&server.uri(), 4);
        let tiles: Vec<Value> = (0..(MAX_BATCH_SIZE + 1))
            .map(|i| json!({ "level": 2, "id": i as u32 }))
            .collect();
        let (status, _) = call(router(state), json!({ "encoding": "zstd", "tiles": tiles })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_tile_rejected_with_400() {
        let server = MockServer::start().await;
        let state = test_state(&server.uri(), 4);
        let (status, _) = call(
            router(state),
            json!({
                "encoding": "zstd",
                "tiles": [{ "level": 99, "id": 0 }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_tile_id_out_of_range_rejected() {
        let server = MockServer::start().await;
        let state = test_state(&server.uri(), 4);
        let (status, _) = call(
            router(state),
            json!({
                "encoding": "zstd",
                "tiles": [{ "level": 0, "id": 99999 }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn all_tiles_errored_returns_502_with_per_tile_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/0/000/529.gph"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let state = test_state(&server.uri(), 4);
        let (status, body) = call(
            router(state),
            json!({
                "encoding": "zstd",
                "tiles": [
                    { "level": 0, "id": 529 },
                    { "level": 2, "id": 818660 }
                ]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let sizes = body.get("sizes").unwrap().as_array().unwrap();
        assert_eq!(sizes.len(), 2);
        assert!(sizes[0]["error"].is_string());
        assert!(sizes[1]["error"].is_string());
    }

    #[tokio::test]
    async fn partial_failures_still_return_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/0/000/529.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 11]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let state = test_state(&server.uri(), 4);
        let (status, _body) = call(
            router(state),
            json!({
                "encoding": "zstd",
                "tiles": [
                    { "level": 0, "id": 529 },
                    { "level": 2, "id": 818660 }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn upstream_5xx_returns_per_tile_error_without_poisoning_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/0/000/529.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 7]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let state = test_state(&server.uri(), 4);
        let cache = state.cache.clone();

        let (status, body) = call(
            router(state),
            json!({
                "encoding": "zstd",
                "tiles": [
                    { "level": 0, "id": 529 },
                    { "level": 2, "id": 818660 }
                ]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let sizes = body.get("sizes").unwrap().as_array().unwrap();
        assert_eq!(sizes.len(), 2);
        assert_eq!(sizes[0]["bytes"], 7);
        assert_eq!(sizes[0]["missing"], false);
        assert!(sizes[0].get("error").is_none());
        assert_eq!(sizes[1]["bytes"], Value::Null);
        assert_eq!(sizes[1]["missing"], false);
        assert!(sizes[1]["error"].is_string());

        assert_eq!(
            cache
                .get(&CacheKey {
                    level: 0,
                    tile_id: 529,
                    encoding: Encoding::Zstd,
                })
                .map(|v| *v.value()),
            Some(Some(7))
        );
        assert!(
            cache
                .get(&CacheKey {
                    level: 2,
                    tile_id: 818660,
                    encoding: Encoding::Zstd,
                })
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_fetches_preserve_input_order_under_delays() {
        let server = MockServer::start().await;

        const N: u32 = 30;
        const BASE_ID: u32 = 100_000;
        for i in 0..N {
            let tile_id = BASE_ID + i;
            let dir = tile_id / 1000;
            let file = tile_id % 1000;
            let path = format!("/tiles/2/{dir:03}/{file:03}.gph");
            let delay_ms = (N - i) as u64;
            Mock::given(method("GET"))
                .and(match_path(path))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(vec![0u8; tile_id as usize])
                        .set_delay(Duration::from_millis(delay_ms)),
                )
                .mount(&server)
                .await;
        }

        let state = test_state(&server.uri(), 8);
        let tiles: Vec<Value> = (0..N)
            .map(|i| json!({ "level": 2, "id": BASE_ID + i }))
            .collect();
        let (status, body) =
            call(router(state), json!({ "encoding": "zstd", "tiles": tiles })).await;

        assert_eq!(status, StatusCode::OK);
        let sizes = body.get("sizes").unwrap().as_array().unwrap();
        assert_eq!(sizes.len(), N as usize);
        for (i, item) in sizes.iter().enumerate() {
            assert_eq!(item["level"], 2);
            assert_eq!(item["id"], BASE_ID + i as u32);
            assert_eq!(item["bytes"], BASE_ID + i as u32);
        }
    }

    // ---- CLI / server ----

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
    async fn health_returns_ok() {
        let server = MockServer::start().await;
        let app = build_router(test_state(&server.uri(), 4));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
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

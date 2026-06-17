use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::cache::{CacheKey, SizeCache};
use crate::tiles::{Encoding, TileId};
use crate::upstream::{FetchOutcome, RatiClient};

// Soft cap: country-mode level-2 selections are in the low thousands; 20k is a
// comfortable ceiling that fits inside axum's default 2 MiB body limit and
// prevents accidental runaway fetches. Not a hard correctness boundary.
pub const MAX_BATCH_SIZE: usize = 20_000;

#[derive(Clone)]
pub struct AppState {
    pub rati: Arc<RatiClient>,
    pub cache: Arc<SizeCache>,
    /// Bounds upstream fan-out globally across all concurrent requests.
    pub upstream_permits: Arc<Semaphore>,
    /// Configured concurrency value, kept for display/health.
    pub concurrency: usize,
}

#[derive(Debug, Deserialize)]
pub struct TileRef {
    pub level: u8,
    pub id: u32,
}

#[derive(Debug, Deserialize)]
pub struct TileSizesRequest {
    pub encoding: Encoding,
    pub tiles: Vec<TileRef>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TileSize {
    pub level: u8,
    pub id: u32,
    pub bytes: Option<u64>,
    pub cached: bool,
    pub missing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TileSizesResponse {
    pub encoding: &'static str,
    pub sizes: Vec<TileSize>,
}

pub async fn tile_sizes(
    State(state): State<AppState>,
    Json(req): Json<TileSizesRequest>,
) -> Result<Json<TileSizesResponse>, (StatusCode, String)> {
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

    let rati = state.rati.clone();
    let cache = state.cache.clone();
    let permits = state.upstream_permits.clone();
    // bound the per-request fan-out by the global permit count too — no reason
    // to spawn more futures than will ever run concurrently.
    let buffer_size = permits.available_permits().max(1).min(misses.len().max(1));

    let fetched: Vec<(usize, TileId, Result<FetchOutcome, String>)> =
        futures::stream::iter(misses.into_iter().map(|(idx, tile)| {
            let rati = rati.clone();
            let permits = permits.clone();
            async move {
                let _permit = permits
                    .acquire()
                    .await
                    .expect("upstream semaphore is never closed");
                let outcome = rati
                    .fetch_size(tile, encoding)
                    .await
                    .map_err(|e| e.to_string());
                (idx, tile, outcome)
            }
        }))
        .buffer_unordered(buffer_size)
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
                // Don't poison the cache on transient upstream failures; just
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

    let sizes = results.into_iter().map(|r| r.expect("filled")).collect();
    Ok(Json(TileSizesResponse {
        encoding: encoding.as_header_value(),
        sizes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::DefaultBodyLimit,
        http::Request,
        routing::{MethodRouter, post},
    };
    use pretty_assertions::assert_eq;
    use rustc_hash::FxBuildHasher;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path as match_path};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    fn tile_sizes_route() -> MethodRouter<AppState> {
        post(tile_sizes).layer(DefaultBodyLimit::max(4 * 1024 * 1024))
    }

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/api/tile-sizes", tile_sizes_route())
            .with_state(state)
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

    fn make_state(base_url: &str, concurrency: usize) -> AppState {
        AppState {
            rati: Arc::new(RatiClient::new(base_url.to_string()).unwrap()),
            cache: Arc::new(SizeCache::with_hasher(FxBuildHasher)),
            upstream_permits: Arc::new(Semaphore::new(concurrency)),
            concurrency,
        }
    }

    #[tokio::test]
    async fn empty_batch_returns_empty_sizes() {
        let server = MockServer::start().await;
        let state = make_state(&server.uri(), 4);
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

        let state = make_state(&server.uri(), 4);
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
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 2435]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/1/051/234.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 999]))
            .mount(&server)
            .await;

        let state = make_state(&server.uri(), 4);
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
        assert_eq!(sizes[0]["missing"], false);
        assert_eq!(sizes[1]["level"], 2);
        assert_eq!(sizes[1]["id"], 818660);
        assert_eq!(sizes[1]["bytes"], 2435);
        assert_eq!(sizes[1]["cached"], false);
        assert_eq!(sizes[1]["missing"], false);
        assert_eq!(sizes[2]["level"], 1);
        assert_eq!(sizes[2]["id"], 51234);
        assert_eq!(sizes[2]["bytes"], 999);
        assert_eq!(sizes[2]["cached"], false);
        assert_eq!(sizes[2]["missing"], false);
    }

    #[tokio::test]
    async fn missing_tile_propagates_as_null_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let state = make_state(&server.uri(), 4);
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
        let state = make_state(&server.uri(), 4);
        let tiles: Vec<Value> = (0..(MAX_BATCH_SIZE + 1))
            .map(|i| json!({ "level": 2, "id": i as u32 }))
            .collect();
        let (status, _) = call(router(state), json!({ "encoding": "zstd", "tiles": tiles })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_tile_rejected_with_400() {
        let server = MockServer::start().await;
        let state = make_state(&server.uri(), 4);
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
        let state = make_state(&server.uri(), 4);
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
    async fn upstream_5xx_returns_per_tile_error_without_poisoning_cache() {
        let server = MockServer::start().await;
        // tile 0/000/529: succeeds with 200 OK / 7 bytes
        Mock::given(method("GET"))
            .and(match_path("/tiles/0/000/529.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 7]))
            .mount(&server)
            .await;
        // tile 2/818/660: always fails 500
        Mock::given(method("GET"))
            .and(match_path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let state = make_state(&server.uri(), 4);
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

        // batch returns 200, the failing tile carries an error string
        assert_eq!(status, StatusCode::OK);
        let sizes = body.get("sizes").unwrap().as_array().unwrap();
        assert_eq!(sizes.len(), 2);
        assert_eq!(sizes[0]["bytes"], 7);
        assert_eq!(sizes[0]["missing"], false);
        assert!(sizes[0].get("error").is_none());
        assert_eq!(sizes[1]["bytes"], Value::Null);
        assert_eq!(sizes[1]["missing"], false);
        assert!(sizes[1]["error"].is_string());

        // cache holds the successful tile…
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
        // …but is NOT poisoned with the failed one.
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

        // 30 distinct level-2 tiles. The earliest in the input list get the
        // longest upstream delay; if results were returned in completion order
        // (rather than rebound to input index), the assertion below would fail.
        const N: u32 = 30;
        const BASE_ID: u32 = 100_000;
        for i in 0..N {
            let tile_id = BASE_ID + i;
            let dir = tile_id / 1000;
            let file = tile_id % 1000;
            let path = format!("/tiles/2/{dir:03}/{file:03}.gph");
            // delay shrinks from N-i down to 0 ms, so input position 0 finishes last
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

        let state = make_state(&server.uri(), 8);
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
}

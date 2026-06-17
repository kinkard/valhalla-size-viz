use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::cache::{CacheKey, SizeCache};
use crate::tiles::{Encoding, TileId};
use crate::upstream::{FetchOutcome, RatiClient};

// Soft cap: country-mode level-2 selections are in the low thousands; 50k is a
// comfortable ceiling that prevents accidental runaway fetches without being a
// hard correctness boundary.
const MAX_BATCH_SIZE: usize = 50_000;

#[derive(Clone)]
pub struct AppState {
    pub rati: Arc<RatiClient>,
    pub cache: Arc<SizeCache>,
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
        if let Some(cached) = state.cache.get(key) {
            results[idx] = Some(TileSize {
                level: tile.level,
                id: tile.id,
                bytes: cached,
                cached: true,
                missing: cached.is_none(),
            });
        } else {
            misses.push((idx, tile));
        }
    }

    let rati = state.rati.clone();
    let cache = state.cache.clone();
    let concurrency = state.concurrency.max(1);

    let fetched: Vec<(usize, TileId, Result<FetchOutcome, String>)> =
        futures::stream::iter(misses.into_iter().map(|(idx, tile)| {
            let rati = rati.clone();
            async move {
                let outcome = rati
                    .fetch_size(tile, encoding)
                    .await
                    .map_err(|e| e.to_string());
                (idx, tile, outcome)
            }
        }))
        .buffer_unordered(concurrency)
        .collect()
        .await;

    for (idx, tile, outcome) in fetched {
        let key = CacheKey {
            level: tile.level,
            tile_id: tile.id,
            encoding,
        };
        let (bytes, missing) = match outcome {
            Ok(FetchOutcome::Found(b)) => {
                cache.insert(key, Some(b));
                (Some(b), false)
            }
            Ok(FetchOutcome::Missing) => {
                cache.insert(key, None);
                (None, true)
            }
            Err(err) => {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    format!("upstream fetch failed for {}: {err}", tile.to_path()),
                ));
            }
        };
        results[idx] = Some(TileSize {
            level: tile.level,
            id: tile.id,
            bytes,
            cached: false,
            missing,
        });
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
    use axum::{Router, body::Body, http::Request, routing::post};
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;
    use wiremock::matchers::{method, path as match_path};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/api/tile-sizes", post(tile_sizes))
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
            cache: Arc::new(SizeCache::new()),
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
            cache.get(CacheKey {
                level: 2,
                tile_id: 818660,
                encoding: Encoding::Zstd,
            }),
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
}

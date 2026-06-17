use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;
use valhalla_size_viz::{AppState, RatiClient, SizeCache, build_router};

fn test_state() -> AppState {
    AppState {
        rati: Arc::new(RatiClient::new("http://localhost:0".to_string()).unwrap()),
        cache: Arc::new(SizeCache::new()),
        concurrency: 32,
    }
}

#[tokio::test]
async fn serves_index_html() {
    let app = build_router(test_state());
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!bytes.is_empty(), "served HTML body is empty");
    let body = std::str::from_utf8(&bytes).expect("body is UTF-8");
    assert!(
        body.contains("Valhalla Tile Size Visualizer"),
        "served HTML missing expected title"
    );
}

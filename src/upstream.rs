use std::sync::Arc;

use futures::StreamExt;
use reqwest::{StatusCode, header};
use tracing::warn;

use crate::tiles::{Encoding, TileId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    Found(u64),
    Missing,
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("upstream returned status {status} for {path}")]
    Upstream { status: u16, path: String },
}

pub struct RatiClient {
    http: reqwest::Client,
    base_url: Arc<str>,
}

impl RatiClient {
    pub fn new(base_url: impl Into<Arc<str>>) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into(),
        })
    }

    pub async fn fetch_size(
        &self,
        tile: TileId,
        encoding: Encoding,
    ) -> Result<FetchOutcome, FetchError> {
        let path = tile.to_path();
        let url = format!("{}/tiles/{}", self.base_url.trim_end_matches('/'), path);

        let response = self
            .http
            .get(&url)
            .header(header::ACCEPT_ENCODING, encoding.as_header_value())
            .send()
            .await?;

        let status = response.status();
        if status == StatusCode::NOT_FOUND {
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

        let mut stream = response.bytes_stream();
        let mut bytes: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            bytes += chunk.len() as u64;
        }
        Ok(FetchOutcome::Found(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use wiremock::matchers::{header as match_header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tile() -> TileId {
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
            .and(path("/tiles/2/818/660.gph"))
            .and(match_header("accept-encoding", "zstd"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "zstd")
                    .set_body_bytes(body.clone()),
            )
            .mount(&server)
            .await;

        let client = RatiClient::new(server.uri()).unwrap();
        let outcome = client.fetch_size(tile(), Encoding::Zstd).await.unwrap();
        assert_eq!(outcome, FetchOutcome::Found(1234));
    }

    #[tokio::test]
    async fn fetch_404_returns_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tiles/2/818/660.gph"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = RatiClient::new(server.uri()).unwrap();
        let outcome = client.fetch_size(tile(), Encoding::Gzip).await.unwrap();
        assert_eq!(outcome, FetchOutcome::Missing);
    }

    #[tokio::test]
    async fn fetch_500_returns_upstream_error_with_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tiles/0/000/529.gph"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = RatiClient::new(server.uri()).unwrap();
        let tile = TileId { level: 0, id: 529 };
        let err = client
            .fetch_size(tile, Encoding::Identity)
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
        let body = vec![0u8; 777];
        Mock::given(method("GET"))
            .and(path("/tiles/2/818/660.gph"))
            .and(match_header("accept-encoding", "zstd"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(body),
            )
            .mount(&server)
            .await;

        let client = RatiClient::new(server.uri()).unwrap();
        let outcome = client.fetch_size(tile(), Encoding::Zstd).await.unwrap();
        assert_eq!(outcome, FetchOutcome::Found(777));
    }

    #[tokio::test]
    async fn fetch_identity_with_no_content_encoding_header_is_not_a_mismatch() {
        let server = MockServer::start().await;
        let body = vec![0u8; 42];
        Mock::given(method("GET"))
            .and(path("/tiles/0/000/529.gph"))
            .and(match_header("accept-encoding", "identity"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;

        let client = RatiClient::new(server.uri()).unwrap();
        let tile = TileId { level: 0, id: 529 };
        let outcome = client.fetch_size(tile, Encoding::Identity).await.unwrap();
        assert_eq!(outcome, FetchOutcome::Found(42));
    }

    #[tokio::test]
    async fn base_url_trailing_slash_is_tolerated() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tiles/0/000/529.gph"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 3]))
            .mount(&server)
            .await;

        let client = RatiClient::new(format!("{}/", server.uri())).unwrap();
        let tile = TileId { level: 0, id: 529 };
        let outcome = client.fetch_size(tile, Encoding::Identity).await.unwrap();
        assert_eq!(outcome, FetchOutcome::Found(3));
    }
}

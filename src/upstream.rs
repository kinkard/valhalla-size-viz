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
    #[error("upstream returned 200 OK without Content-Length for {path}")]
    MissingContentLength { path: String },
}

pub struct RatiClient {
    http: reqwest::Client,
    base_url: String,
}

impl RatiClient {
    pub fn new(base_url: String) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .build()?;
        Ok(Self { http, base_url })
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

        // rati always sets Content-Length on tile GETs (axum derives it from the
        // finite Bytes body it returns). If it's missing, something is wrong
        // upstream — surface it rather than silently misreport the size.
        match response.content_length() {
            Some(len) => Ok(FetchOutcome::Found(len)),
            None => Err(FetchError::MissingContentLength { path }),
        }
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
            // Drain request headers (read until \r\n\r\n).
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
            // Respond with chunked transfer encoding, no Content-Length.
            let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Encoding: identity\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
            sock.write_all(response).await.unwrap();
            sock.flush().await.unwrap();
        });

        let client = RatiClient::new(format!("http://{addr}")).unwrap();
        let tile = TileId { level: 0, id: 529 };
        let err = client
            .fetch_size(tile, Encoding::Identity)
            .await
            .unwrap_err();
        match err {
            FetchError::MissingContentLength { path } => {
                assert_eq!(path, "0/000/529.gph");
            }
            other => panic!("expected MissingContentLength, got {other:?}"),
        }
    }
}

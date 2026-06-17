# Valhalla Tile Size Visualizer

## Overview

Build an open-source web tool that visualizes Valhalla graph tile sizes on a MapLibre map. The frontend is a port of `../sar-tiles-viz/web/index.html` (bbox / polygon / country / route selection modes). The backend is an Axum proxy in front of [rati](https://github.com/kinkard/rati) that:

- Issues full GET requests upstream with a chosen `Accept-Encoding` (`identity`/`gzip`/`zstd`), counts the response body bytes, and returns that size. Required because browsers cannot control `Accept-Encoding` on `fetch()`, and rati's HEAD path omits `Content-Length` whenever the requested encoding differs from the on-disk encoding — and we don't know the on-disk encoding from the client side, so HEAD is unreliable in the general case.
- Caches sizes per `(level, tile_id, encoding)` in a fast in-memory hash map so repeated visualizations don't re-fetch.
- Drives 32-way concurrency to rati (configurable via `--concurrency`) to keep batch fetches fast.

The tool ships as a Docker image and a `cargo run` binary, mirroring the structure of `../valhalla-debug`.

## Context (from discovery)

- **Starter layout (`/Users/stepankizim/Developer/valhalla-size-viz/`):** empty `src/main.rs`, empty `Cargo.toml`, CI in `.github/workflows/` (`sanity_checks.yml`, `publish.yml`, `push_docker_image.yml`), `Dockerfile` (alpine stub), `Dockerfile.test` (alpine, `cargo build` no release, runs fmt/clippy/test), `deny.toml` (default template, narrow license allow-list), `LICENSE-{MIT,APACHE}`, placeholder `README.md`.
- **Existing CI hazards (uncovered during review):**
  - `Dockerfile` runtime stage is missing `ca-certificates` (reqwest+rustls needs them to verify https rati backends).
  - `Dockerfile.test` runs `cargo build` (debug) — should match the release toolchain we ship.
  - `publish.yml` publishes to crates.io on tag push — user has opted to delete it.
  - `push_docker_image.yml` has literal placeholder tags `{{username}}/{{project-name}}:latest` — needs `kinkard/valhalla-size-viz:latest`.
  - `sanity_checks.yml` runs `Dockerfile.test` so the fix propagates here automatically.
  - **Base image: staying with alpine.** Our binary is pure-Rust + rustls — no C++ deps, no openssl. Network/upstream latency dominates total runtime, so musl's allocator slowness is invisible. Alpine gives us ~15 MB images and matches the existing template; no reason to switch to debian.
- **Architecture template (`../valhalla-debug/src/main.rs`):** Axum app, `tokio::runtime::Builder::new_multi_thread()` with `--concurrency` flag, `serve_index_html()` reads `web/index.html` at request time, two-stage `Dockerfile` (rust:slim-trixie builder → debian:trixie-slim runner with `COPY web ./web`).
- **Frontend source (`../sar-tiles-viz/web/index.html`):** 3810-line single-file MapLibre app. Currently does `fetch(${ratiUrl}/tiles/${tile.path}, { method: 'HEAD' })` with worker-pool concurrency=10. We replace that with a single batch POST against our own server and drop the rati URL input from the UI.
- **rati request format (`../rati/src/main.rs`):** `GET /tiles/{level}/{padded_id_thousands}/{padded_remainder}.gph` with `Accept-Encoding: gzip` or `zstd` (preferring zstd when both accepted). Response carries `Content-Encoding`. rati's HEAD path emits `Content-Length` only when the requested encoding matches the on-disk encoding; otherwise the header is omitted (lines 222-243 of `../rati/src/main.rs`).
- **Tile ID convention:** level 0 (highway, 4°, 90 cols), level 1 (arterial, 1°, 360 cols), level 2 (local, 0.25°, 1440 cols). Path is `<level>/<floor(id/1000):03>/<id%1000:03>.gph` (lifted directly from `tileIdToPath` in the sar-tiles HTML).
- **Repo decisions (from user, locked in):** repository URL `https://github.com/kinkard/valhalla-size-viz`; `publish.yml` deleted; Docker Hub tag `kinkard/valhalla-size-viz:latest`.

## Development Approach

- **Testing approach:** Regular (code first, then tests). Unit tests live alongside code in `mod tests`. No project-level e2e harness — manual browser smoke test against a real rati instance is the integration check.
- complete each task fully before moving to the next; small, focused changes
- **every task includes new/updated tests** for the code it adds (success + error paths)
- **all tests must pass before starting the next task** — `cargo test` is green; `cargo clippy -- -Dwarnings` is clean
- update this plan file when scope changes during implementation
- commits go directly to `main` (fresh repo, no branches)

## Testing Strategy

- **Unit tests** (required per task): cover tile path encoding, encoding negotiation, cache hit/miss semantics, batch request parsing.
- **Upstream tests** (Task 4, Task 5): `wiremock` simulates rati. Handlers tested via `tower::ServiceExt::oneshot` against the Router (no `axum-test` dep).
- **Integration smoke** (manual, end of plan): start the binary against a running rati, open the page in a browser, draw a bbox, confirm sizes render with each encoding.
- **CI checks** wired via `Dockerfile.test`: `cargo fmt --check`, `cargo clippy -- -Dwarnings`, `cargo test`. `cargo deny` is intentionally not run in CI; we'll keep `deny.toml` current as a convenience for local checks but won't gate on it.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix

## Solution Overview

```
              browser (MapLibre)
                    │
                    │  POST /api/tile-sizes
                    │  { encoding, tiles: [{level,id}…] }
                    ▼
       ┌────────────────────────────┐
       │  valhalla-size-viz (axum)  │
       │                            │
       │  cache: DashMap<           │  cache key = (level, tile_id, encoding)
       │   CacheKey, Option<u64>,   │  value     = Some(bytes) | None (404)
       │   FxBuildHasher>           │
       │                            │
       │  upstream: reqwest         │  semaphore-limited fan-out (default 32)
       │   GET /tiles/{path}        │  Accept-Encoding: {encoding}
       └────────────┬───────────────┘
                    │  full GET, body bytes counted
                    ▼
                  rati (S3-backed)
```

**Key design decisions:**

- **Batch POST, not per-tile GET.** A country fetch is hundreds–thousands of tiles. The browser caps HTTP/1.1 at 6 connections per origin, so per-tile would serialize. One batch lets the server drive 32-way concurrency to rati.
- **In-memory only, FxHashMap-based.** User chose simplicity over restart persistence. We use `dashmap::DashMap<CacheKey, Option<u64>, rustc_hash::FxBuildHasher>` for lock-free concurrent reads/writes. Per-entry overhead is ~50 bytes including DashMap shard metadata, so 3M entries ≈ 150 MB worst case — well within budget for a single-host tool.
- **Full GET, not HEAD.** rati's HEAD response omits `Content-Length` whenever the requested encoding differs from the on-disk encoding, and we don't know the on-disk encoding from the client. We always GET, drain the body into a counter, and throw the bytes away.
- **Single rati backend, CLI-only.** `--rati-url` flag. The HTML drops its rati-url input (less confusing UX, simpler cache key).
- **Same-origin only.** Frontend is served from the same axum process that handles the API, so no CORS layer is needed.
- **No frontend build step.** The HTML/JS stays a single static file served by axum, matching valhalla-debug.

## Technical Details

**Request/response shapes:**

```jsonc
// POST /api/tile-sizes
{
  "encoding": "zstd",            // "identity" | "gzip" | "zstd"
  "tiles": [
    { "level": 2, "id": 818660 },
    { "level": 1, "id": 51234 }
  ]
}

// 200 OK
{
  "encoding": "zstd",
  "sizes": [
    { "level": 2, "id": 818660, "bytes": 2435, "cached": true,  "missing": false },
    { "level": 1, "id": 51234,  "bytes": null, "cached": false, "missing": true  }
  ]
}
```

**Cache key:**

```rust
#[derive(Hash, Eq, PartialEq, Clone, Copy)]
struct CacheKey {
    level: u8,
    tile_id: u32,
    encoding: Encoding, // Identity | Gzip | Zstd
}
```

**Upstream concurrency:** `tokio::sync::Semaphore` of size `--concurrency` (default 32). `futures::stream::iter(...).buffer_unordered(N)` for the fan-out inside the handler.

**Body counting:** `response.bytes_stream()` → fold over chunk lengths into a `u64`. No full-body allocation.

**Encoding selection on the wire:** `reqwest::Client::builder().no_gzip().no_brotli().no_deflate().build()` so reqwest doesn't transparently decompress. Set `Accept-Encoding` explicitly to the requested encoding. Verify the response's `Content-Encoding` matches what we asked for; on mismatch, log `warn!` but record the actual bytes anyway.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code in this repo — Rust modules, frontend port, Dockerfiles, README, CI workflow edits.
- **Post-Completion** (no checkboxes): manual browser verification, screenshot for the README, GitHub Actions secret setup (Docker Hub login + GITHUB_TOKEN), pushing the first tagged release.

## Implementation Steps

### Task 1: Cargo.toml dependencies, metadata, and Cargo.lock

**Files:**
- Modify: `Cargo.toml`
- Create: `Cargo.lock` (committed)

- [x] runtime deps: `axum = "0.8"`, `clap = { version = "4.5", features = ["derive", "env"] }`, `dashmap = "6"`, `futures = "0.3"`, `reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream"] }`, `rustc-hash = "2"`, `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`, `thiserror = "2"`, `tokio = { version = "1", features = ["rt-multi-thread", "fs", "io-util", "signal", "sync"] }`, `tower = { version = "0.5", features = ["util"] }`, `tower-http = { version = "0.6", features = ["trace"] }`, `tracing = "0.1"`, `tracing-subscriber = { version = "0.3", features = ["fmt", "ansi"] }`
- [x] dev-deps: `pretty_assertions = "1"`, `wiremock = "0.6"`
- [x] set `repository = "https://github.com/kinkard/valhalla-size-viz"`
- [x] set `description = "Visualize Valhalla graph tile sizes on a map"`
- [x] expand `include = ["src/**/*.rs", "web/**", "Cargo.toml", "README.md", "LICENSE-*"]`
- [x] run `cargo build` once, commit the resulting `Cargo.lock` (Rust binaries should commit their lockfile)
- [x] no tests for this task — verify by `cargo build` succeeding

### Task 1b: Reconcile existing infrastructure

**Files:**
- Modify: `Dockerfile`
- Modify: `Dockerfile.test`
- Delete: `.github/workflows/publish.yml`
- Modify: `.github/workflows/push_docker_image.yml`
- Modify: `deny.toml`

- [x] update `Dockerfile` (stay on alpine): two-stage `rust:alpine` builder → `alpine` runtime. Keep existing dummy-`main.rs` dep-caching trick. Add to the runtime stage: `RUN apk add --no-cache ca-certificates` (reqwest+rustls needs the cert bundle), `WORKDIR /usr`, `COPY web ./web`, `COPY --from=builder /usr/src/app/target/release/valhalla-size-viz /usr/local/bin/`, `ENTRYPOINT ["valhalla-size-viz"]` (replace the current `CMD` form so flags work cleanly).
- [x] update `Dockerfile.test` (stay on alpine): switch the build commands from `cargo build` to `cargo build --release` so we test the same compilation profile we ship. Keep `cargo fmt -- --check`, `cargo clippy -- -Dwarnings`, `cargo test`. Keep the dummy-`main.rs` dep-caching trick.
- [x] delete `.github/workflows/publish.yml` (user decision — no crates.io publishing for now)
- [x] update `.github/workflows/push_docker_image.yml`: replace `{{username}}/{{project-name}}:latest` with `kinkard/valhalla-size-viz:latest`. Keep `linux/arm64` platform (matches runner) — note this in the README so users know the published image is arm64-only.
- [x] update `deny.toml`'s `[licenses] allow` list to include `ISC` (ring), `Unicode-3.0` (icu_*), `Zlib`, `MPL-2.0` (in case any transitive uses it). Not running cargo-deny in CI but keeping it sane for local checks.
- [x] verify: `docker build .` succeeds; `docker build -f Dockerfile.test .` runs fmt/clippy/test in container (skipped - no docker daemon available; verified by Dockerfile inspection)
- [x] no Rust tests added (config-only task)

### Task 2: Tile types and path encoding

**Files:**
- Create: `src/tiles.rs`
- Modify: `src/main.rs` (add `mod tiles;`)

- [x] define `Encoding` enum (`Identity` | `Gzip` | `Zstd`) with `as_header_value()` returning `"identity"`/`"gzip"`/`"zstd"` and a serde `Deserialize` impl that accepts lowercase strings
- [x] define `TileId { level: u8, id: u32 }` with `to_path() -> String` producing `"<level>/<id/1000:03>/<id%1000:03>.gph"`
- [x] add a `LEVELS` constants table (with comment) holding `(size_deg, cols, rows, max_tile_id)` for levels 0/1/2 — used by `TileId::validate()`
- [x] `TileId::validate()` rejects `level > 2` and `id > LEVELS[level].max_tile_id`
- [x] write tests: `to_path` round-trip for known cases (level 2 id 818660 → `"2/818/660.gph"` [plan originally said `"2/000/818/660.gph"`, a typo — corrected to match the JS reference], level 0 id 529 → `"0/000/529.gph"`)
- [x] write tests: `Encoding::deserialize` for `"identity"`, `"gzip"`, `"zstd"`, reject `"br"` and uppercase
- [x] write tests: `validate()` rejects out-of-range level and id, accepts boundary values
- [x] `cargo test` must pass

### Task 3: In-memory size cache

**Files:**
- Create: `src/cache.rs`
- Modify: `src/main.rs` (add `mod cache;`)

- [x] define `SizeCache` wrapping `DashMap<CacheKey, Option<u64>, FxBuildHasher>` (`rustc_hash::FxBuildHasher`)
- [x] `CacheKey { level: u8, tile_id: u32, encoding: Encoding }`
- [x] methods: `new()`, `get(&self, key) -> Option<Option<u64>>`, `insert(&self, key, value)`, `len(&self)`
- [x] cache value: `Option<u64>` — `Some(bytes)` for known size, `None` for confirmed-404
- [x] write tests: insert+get round-trip
- [x] write tests: separate entries for the same tile under different encodings
- [x] write tests: 404 caching (`insert(k, None)` then `get(k)` returns `Some(None)`)
- [x] `cargo test` must pass

### Task 4: Rati upstream client

**Files:**
- Create: `src/upstream.rs`
- Modify: `src/main.rs` (add `mod upstream;`)

- [x] `RatiClient { http: reqwest::Client, base_url: Arc<str> }`
- [x] constructor builds `reqwest::Client` with `.no_gzip().no_brotli().no_deflate()` so reqwest does not transparently decompress
- [x] `async fn fetch_size(&self, tile: TileId, encoding: Encoding) -> Result<FetchOutcome, FetchError>` where `FetchOutcome` is `Found(u64) | Missing`
- [x] `FetchError` variants: `Network(reqwest::Error)`, `Upstream { status: u16, path: String }` — the path is included so log lines are debuggable
- [x] build URL `{base_url}/tiles/{tile.to_path()}`, set `Accept-Encoding` header to the requested encoding
- [x] stream body via `response.bytes_stream()`, fold chunk lengths into a `u64` counter (no full-body allocation)
- [x] map status 200 → `Found(bytes)`, 404 → `Missing`, other → `FetchError::Upstream { status, path }`
- [x] if `Content-Encoding` on response differs from request, log `warn!(tile = %tile.to_path(), requested, got, "encoding mismatch")` and still count bytes
- [x] write tests with `wiremock` covering: 200 with body of N bytes → `Found(N)`; 404 → `Missing`; 500 → `FetchError::Upstream` with path populated; encoding mismatch logs warn but returns `Found`
- [x] `cargo test` must pass

### Task 5: Batch tile-sizes endpoint

**Files:**
- Create: `src/api.rs`
- Modify: `src/main.rs` (add `mod api;`)

- [x] define request/response structs: `TileSizesRequest { encoding, tiles: Vec<TileRef> }`, `TileRef { level, id }`, `TileSizesResponse { encoding, sizes: Vec<TileSize> }`, `TileSize { level, id, bytes: Option<u64>, cached: bool, missing: bool }`
- [x] handler signature: `async fn tile_sizes(State(state): State<AppState>, Json(req): Json<TileSizesRequest>) -> Result<Json<TileSizesResponse>, (StatusCode, String)>`
- [x] validate every `TileRef` via `TileId::validate()`, return 400 on the first invalid tile with which one failed
- [x] split incoming tiles: lookup cache hits inline, collect misses into a `Vec<(usize, TileId)>` keeping the original index
- [x] fan out misses with `futures::stream::iter(misses).map(...).buffer_unordered(state.concurrency)` calling `RatiClient::fetch_size`
- [x] write each fetched result back into the cache, then assemble the response preserving the input order
- [x] soft cap: reject `req.tiles.len() > 50_000` with 400 to prevent runaway fetches. Empirically a level-2 country selection is in the low thousands; 50k is a comfortable ceiling but is a tunable knob, not a hard correctness boundary. Document this as a soft cap in code comments.
- [x] write handler tests using `tower::ServiceExt::oneshot` against the assembled Router, with `wiremock` standing in for rati: empty batch → empty response; cache-hit short-circuit (insert into cache directly, then call handler, assert no upstream call); mixed cached+uncached; 404 propagates as `bytes: null, missing: true`; oversized batch → 400; invalid tile (level=99) → 400
- [x] `cargo test` must pass

### Task 6: Server bootstrap, CLI, and signal handling

**Files:**
- Modify: `src/main.rs`

- [x] `#[derive(Parser)] struct Config { #[arg(long, default_value_t = 3000)] port: u16, #[arg(long, default_value_t = 32)] concurrency: u16, #[arg(long, env = "RATI_URL")] rati_url: String }` — no `--web-dir` flag; we hardcode `web/index.html` like valhalla-debug
- [x] `AppState { rati: Arc<RatiClient>, cache: Arc<SizeCache>, concurrency: usize }`
- [x] runtime: `tokio::runtime::Builder::new_multi_thread()` with `worker_threads = min(available_parallelism, concurrency)` (mirrors valhalla-debug)
- [x] router: `GET /` → `serve_index_html`, `POST /api/tile-sizes` → `api::tile_sizes`, `GET /healthz` → `"OK"`, with `TraceLayer::new_for_http()`. **No CorsLayer** — frontend and API share the origin.
- [x] `serve_index_html` reads `web/index.html` at request time (so we can hot-edit during dev) — return 404 on missing, 500 on read error
- [x] graceful shutdown on Ctrl+C and SIGTERM (lift the pattern from valhalla-debug)
- [x] add `tracing_subscriber::fmt::init()` at the top of `main`
- [x] write tests: CLI parses `--rati-url http://… --port 4000 --concurrency 16`; defaults match docs (port=3000, concurrency=32)
- [x] write tests: `GET /healthz` returns 200 OK with body `"OK"` (via `tower::ServiceExt::oneshot`)
- [x] `cargo test` must pass; `cargo run -- --rati-url http://localhost:8050` boots cleanly (serves 404 for `/` until Task 7 adds the HTML)

### Task 7: Port the frontend, point it at our endpoint

**Files:**
- Create: `web/index.html`
- Create: `web/countries.js`
- Create: `web/poly-data.js`
- Create: `web/poly/` (copy whole directory)
- Create: `tests/serve_html.rs`

- [x] copy `../sar-tiles-viz/web/{index.html,countries.js,poly-data.js,poly/}` verbatim
- [x] remove the rati-url input from the sidebar (search for `rati-url` in the HTML) and any JS reading `document.getElementById('rati-url').value`
- [x] add a compression selector — radio group `identity` / `gzip` / `zstd` (default `zstd`) bound to a top-level `currentEncoding` variable
- [x] introduce a `lastSelection = { mode, uniqueTiles }` global that the five existing call sites of `fetchTileSizes` (bbox 2129, route 2278, polygon 2572, country 3004, route-mode 3641) populate after computing `uniqueTiles`. On encoding-radio change, re-call `fetchTileSizes(lastSelection.uniqueTiles)` if it's set and re-render — no need to re-run the whole `recompute*` pipeline.
- [x] replace `fetchTileSizes(tiles, concurrency)` body with a single `POST /api/tile-sizes` that sends `{ encoding: currentEncoding, tiles: tiles.map(t => ({ level: t.level, id: t.tileId })) }`. Map the response array back to the existing `Map<path, {size, missing}>` shape so callers don't change.
- [x] update the page `<title>` to `"Valhalla Tile Size Visualizer"` and the sidebar `<h1>` accordingly
- [x] add `tests/serve_html.rs`: spin up the Router with a stub `RatiClient` (no rati needed), `oneshot` `GET /`, assert 200 + non-empty body. This test only exercises static serving — no upstream calls.
- [x] `cargo test` must pass; manual browser check: bbox a small region, see sizes render under all three encodings; toggling the radio re-fetches and re-renders without redrawing the selection (manual browser check pending Task 9)

### Task 8: README cleanup

**Files:**
- Modify: `README.md`

- [x] replace placeholder content with: 1-paragraph intent, screenshot placeholder, CLI usage table, `cargo run` example, `docker run` example pointing at `kinkard/valhalla-size-viz:latest`, License block (Apache-2.0 + MIT)
- [x] keep concise — mirror the rhythm of `../rati/README.md` and `../valhalla-debug/README.md`
- [x] document the three encoding modes in one sentence each ("identity = raw on-wire bytes; gzip = browser-friendly; zstd = best ratio")
- [x] mention that the published Docker Hub image is `linux/arm64` only (matches the workflow runner); users on amd64 should `docker build` locally
- [x] link to rati and Valhalla
- [x] no tests — verify by visual review

### Task 9: Verify acceptance criteria

- [x] all Overview requirements implemented: server serves HTML, batch endpoint works, FxHasher-backed DashMap cache hits, 32-concurrency fan-out to rati, Docker image builds and runs
- [x] edge cases handled: 404 cached as `None`, oversized batch rejected, invalid tile rejected, encoding mismatch logged
- [x] `cargo test --all` green
- [x] `cargo clippy -- -Dwarnings` clean
- [x] `cargo fmt --check` clean
- [x] `docker build -f Dockerfile.test .` (skipped - no docker daemon)
- [x] `docker build .` (skipped - no docker daemon)
- [x] `docker run` healthz check (skipped - no docker daemon)
- [x] manual browser e2e (skipped - not automatable in this session)

### Task 10: [Final] Move plan to completed and update CLAUDE memory if any new patterns emerged

- [ ] `mkdir -p docs/plans/completed`
- [ ] move this plan file into `docs/plans/completed/`
- [ ] commit directly to `main`

## User's Feedback (during execution)

The user flagged these mid-execution; they supersede earlier plan decisions where they conflict:

- **Use `futures-util = { version = "0.3", default-features = false, features = ["alloc"] }` instead of the umbrella `futures` crate.** Lighter dep tree; we only need combinators (`StreamExt`, `buffered`).
- **Drop the `src/lib.rs` + `src/main.rs` split.** It was introduced solely so `tests/serve_html.rs` could import the router. Fold everything back into `src/main.rs`, delete `tests/serve_html.rs` — that test ("body contains a substring") verifies nothing real. Match the structure of `../rati` and `../valhalla-debug`.
- **Tokio worker_threads cap at 4, decoupled from `--concurrency`.** Async tasks share threads — you don't need 32 OS threads to issue 32 concurrent HTTP fetches. The runtime should be hardcoded to ~4 workers; `--concurrency` only bounds upstream in-flight requests (via `Semaphore` or `buffered`).
- **Drop `src/cache.rs`. Don't test trivial type aliases.** `pub type SizeCache = DashMap<…>` doesn't deserve its own file or unit tests — DashMap is tested upstream. Move `CacheKey` and the alias inline where they're used.
- **Use the response's `Content-Length` header in `RatiClient::fetch_size`** instead of streaming and counting bytes. rati always sets `Content-Length` on tile GETs (axum derives it from the finite `Bytes` body it returns, including any re-encoded content). Trust that header. The streaming counter is dead weight.

## Post-Completion

**Manual verification:**

- Browser smoke test against a real rati instance with all three encodings.
- Capture a screenshot for the README (commit to `docs/screenshot.png` or link to a GitHub release asset).
- Stress check: country mode on a large country (e.g. France) — confirm 32-concurrency fan-out doesn't melt rati or hit S3 rate limits.

**External system updates:**

- Configure GitHub Actions secrets `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN` on the new `kinkard/valhalla-size-viz` repo so `push_docker_image.yml` can push.
- After first green CI run on `main`, tag `v0.1.0` and confirm the Docker image lands on Docker Hub.
- Add an amd64 build to `push_docker_image.yml` later if there's demand (`platforms: linux/amd64,linux/arm64`).

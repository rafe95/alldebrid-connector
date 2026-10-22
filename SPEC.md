# Spec: `alldebrid` crate (v0.1)

Async Rust library for the AllDebrid API. It is context-independent: it knows nothing about
indexers, NAS boxes or where files end up. The connector (`alldebrid-connector`) will be built on
top of it, in the same workspace.

Status: **v0.1 implemented**, verified against the real API on 2026-10-22 (see §14). Anything still
marked ⚠️ has not been confirmed yet
(demo keys: `staticDemoApikeyPrem`, `staticDemoApikeyTria`, `staticDemoApikeyFree`).

---

## 1. Goals and non-goals

**Goals**
- Cover the flow: *upload magnet → track its status → get its files → get direct links*.
- Strong types for responses and errors (no `serde_json::Value` in the public API).
- Respect the API limits (12 req/s, 600 req/min) and retry transient failures.
- Wait for a magnet to become ready, and watch for changes with the *live* mode of `magnet/status`.

**Non-goals (v0.1)**
- Downloading files to disk (the connector does that; see §8).
- User/hosts, PIN flow, saved links, history, vouchers. These come later.
- A synchronous/blocking API.

## 2. Repo layout (workspace)

```
alldebrid-connector/          (repo)
├── Cargo.toml                [workspace]
├── SPEC.md
└── crates/
    ├── alldebrid/            library (this spec)
    └── alldebrid-connector/  binary: indexer → AllDebrid → NAS (later)
```

## 3. Dependencies

| Crate | Purpose |
|---|---|
| `reqwest` (features `json`, `multipart`, `rustls-tls`, no `default-features`) | HTTP |
| `serde`, `serde_json` | (de)serialization |
| `thiserror` | `Error` type |
| `tokio` (`time`, `sync`) | sleeps, semaphores and rate limiter clocks |
| `futures-core` + `async-stream` | `Stream` for the live watcher |
| `tracing` | optional logging |
| `url` | configurable base URL |

Dev: `tokio` (`macros`, `rt-multi-thread`), `wiremock`.
We use rustls to avoid depending on OpenSSL when building for a NAS/ARM.

## 4. Client

```rust
let client = alldebrid::Client::builder()
    .api_key(std::env::var("ALLDEBRID_API_KEY")?)
    .base_url("https://api.alldebrid.com")   // optional, useful for tests
    .user_agent("alldebrid-connector/0.1")   // optional
    .timeout(Duration::from_secs(30))
    .retry(RetryPolicy::default())           // see §6
    .rate_limit(RateLimit::default())        // 12/s, 600/min
    .build()?;
```

- `Client` is `Clone + Send + Sync` (an `Arc` inside). The rate limiter is shared across clones.
- Authentication: `Authorization: Bearer <apikey>` header. The `agent` parameter is no longer needed (changelog 2025-01-15).
- Every call is a `POST` with an `application/x-www-form-urlencoded` form, the default method since 2025-01-15. Arrays are sent as `magnets[]=…`.
- Each endpoint has its version (`v4` or `v4.1`) pinned in code, never in configuration.
- A custom `reqwest::Client` can be supplied: `.http_client(reqwest::Client)`.

## 5. Endpoints (v0.1)

### Magnets

| Method | Endpoint | Notes |
|---|---|---|
| `upload_magnets(&[impl AsRef<str>]) -> Vec<MagnetUploadResult>` | `POST /v4/magnet/upload` | URIs or hashes. One result per item: `Ok(UploadedMagnet)` or `Err(ApiError)`. |
| `upload_torrent(name, bytes) -> MagnetUploadResult` | `POST /v4/magnet/upload/file` | multipart `files[]` |
| `upload_torrents(Vec<(name, bytes)>) -> Vec<…>` | same | batched |
| `magnet_status(id) -> Magnet` | `POST /v4.1/magnet/status` (`id`) | |
| `magnets(filter: Option<StatusFilter>) -> Vec<Magnet>` | `POST /v4.1/magnet/status` (`status`) | `Active` / `Ready` / `Expired` / `Error` |
| `magnet_files(&[MagnetId]) -> Vec<MagnetFiles>` | `POST /v4/magnet/files` (`id[]`) | tree → see types |
| `delete_magnet(id)` | `POST /v4/magnet/delete` | |
| `restart_magnet(id)` / `restart_magnets(&[id])` | `POST /v4/magnet/restart` | |
| `live_status(&mut LiveSession) -> LiveUpdate` | `POST /v4.1/magnet/status` (`session`, `counter`) | low-level live-mode primitive |

### Links (the minimum needed to turn files into downloadable URLs)

| Method | Endpoint |
|---|---|
| `unlock(link, password: Option<&str>) -> Unlocked` | `POST /v4/link/unlock` |
| `delayed(id) -> DelayedStatus` | `POST /v4/link/delayed` |

`Unlocked` may carry the direct `link` or a `delayed` id; the helper in §7 handles both cases.

### Escape hatch

`client.raw::<T: DeserializeOwned>(version, path, &form) -> Result<T>`. Works for any endpoint not
wrapped yet, like `api()` in the official PHP library.

## 6. Robustness

**Rate limit:** two *token buckets* (12/s and 600/min) shared by the whole `Client`.
Each request waits for a free token. Configurable or can be disabled.

**Retries:** `RetryPolicy { max_retries: 3, base_delay: 500ms, max_delay: 10s, jitter: true }`.
- Retried: network errors/timeouts, HTTP 429, 502, 503 and 504. `Retry-After` is honoured.
- Business errors (`status: "error"`) are not retried, except `NO_SERVER` / `MAGNET_NO_SERVER` ⚠️.
- Every v0.1 endpoint is retried, including `magnet/upload`: re-uploading the same hash returns the
  same id (verified), so there are no duplicates. `magnet/upload/file` (.torrent) is only retried when
  the request certainly did not arrive (connection error, HTTP 429), since that is not verified.

**Deprecated endpoints:** if a response contains `"deprecated": true`, a `tracing::warn!` is logged.

## 7. High-level helpers

### 7.1 `wait_until_ready`
```rust
let magnet = client
    .wait_until_ready(id, WaitOptions { poll_interval: 5s, timeout: Some(2h) })
    .await?; // Ok(Magnet) when Ready; Err(Error::MagnetFailed{code,..}) on failure; Err(Timeout)
```
Polls `magnet_status(id)`. Stops when `status_code` is terminal (see §9).

### 7.2 Live-mode watcher
```rust
let mut events = client.watch_magnets(WatchOptions { interval: 3s });
while let Some(ev) = events.next().await {
    match ev? {
        MagnetEvent::Added(m) | MagnetEvent::Updated(m) => …,
        MagnetEvent::Ready(m) => …,
        MagnetEvent::Failed(m) => …,
        MagnetEvent::Removed(id) => …,
    }
}
```
- Returns `impl Stream<Item = Result<MagnetEvent>>`.
- Internally: random `session` (u32) and `counter` starting at 0. The first response is `fullsync: true`.
  After that only *deltas* (changed magnets) arrive, applied onto an internal `HashMap<MagnetId, Magnet>`.
- Deletions arrive as a `{"deleted": true, "id": …}` delta, repeated over several consecutive calls;
  `Removed` is emitted only once. If another `fullsync` arrives, ids that disappeared are also `Removed`.
- `Ready` and `Failed` are each emitted **only once**, on the status transition.

### 7.3 `unlock_resolved` (simple, part of v0.1)
`client.unlock_resolved(link) -> DirectLink { url, filename, size }`: calls `unlock` and, if the response
is *delayed*, polls `delayed` every 5 s until the link is available.

## 8. How files reach the NAS (a connector decision, not the library's)

| Option | How | Pros | Cons |
|---|---|---|---|
| **A. API + direct HTTP** (recommended) | `magnet_files` → `unlock_resolved` per file → the connector downloads over HTTPS with `Range` (resumable) | Full control: progress, size verification, filters (e.g. video only, no `sample`), retries, no external dependencies | The downloader has to be written (simple with reqwest) |
| **B. WebDAV + rclone copy** | `https://webdav.debrid.it/`, user = apikey, any non-empty password; `rclone copy alldebrid:magnets/<name> /volume1/…` | Zero download code | Depends on rclone and on how folders are named; weaker error handling and progress reporting |
| **C. Mounted WebDAV** (rclone mount) | Plex/Jellyfin read straight from the mount | Uses no disk space | Not really "downloading"; depends on the network and the rclone cache |

The library provides everything needed for A (B/C need nothing from it beyond the upload).
Proposal: the connector implements A and keeps B as a configurable option.

## 9. Main types (draft)

```rust
pub struct MagnetId(pub u64);

pub struct UploadedMagnet { id: MagnetId, hash: String, name: String, size: u64, ready: bool }

pub struct Magnet {
    id: MagnetId, filename: String, size: u64,
    status: String, status_code: StatusCode,
    downloaded: u64, uploaded: u64, seeders: u32,
    download_speed: u64, upload_speed: u64,
    upload_date: i64, completion_date: Option<i64>,
}

#[non_exhaustive]
pub enum StatusCode {        // ⚠️ meanings to be verified
    InQueue,             // 0
    Downloading,         // 1
    Compressing,         // 2
    Uploading,           // 3
    Ready,               // 4
    Error(u8),           // 5..=15 (upload failure, file too big, no peers, …)
    Unknown(u8),
}
impl StatusCode { fn is_terminal(&self) -> bool; fn is_ready(&self) -> bool; }

pub enum FileNode {          // from the n/s/l/e keys of magnet/files
    File { name: String, size: u64, link: String },
    Dir  { name: String, children: Vec<FileNode> },
}
impl MagnetFiles { fn flatten(&self) -> Vec<FlatFile /* path, size, link */>; }
```

Dates: v0.1 keeps Unix timestamps as `i64`. If needed, `time`/`chrono` can be added behind a *feature*.

## 10. Errors

```rust
#[non_exhaustive]
pub enum Error {
    Api(ApiError),                       // status:"error"
    Http { status: u16, body: String },  // non-2xx HTTP without valid JSON
    Network(reqwest::Error),
    Decode { source: serde_json::Error, body: String },
    RateLimited { retry_after: Option<Duration> }, // after retries are exhausted
    MagnetFailed { id: MagnetId, code: StatusCode, message: String },
    Timeout,
    Config(String),
}

pub struct ApiError { pub code: ApiErrorCode, pub message: String }

#[non_exhaustive]
pub enum ApiErrorCode {
    AuthMissingApikey, AuthBadApikey, AuthBlocked, AuthUserBanned,
    MagnetNoUri, MagnetInvalidUri, MagnetMustBePremium, MagnetNoServer,
    MagnetTooManyActive, MagnetInvalidId, MagnetProcessing, MagnetInvalidFile,
    LinkDown, LinkHostNotSupported, LinkPassProtected, /* … */ MustBePremium,
    Other(String),     // unknown codes never break anything
}
impl ApiErrorCode { fn is_auth(&self) -> bool; fn is_retryable(&self) -> bool; }
```

`AUTH_BLOCKED` shows up when AllDebrid detects access from a new location (it must be approved by
email). It will probably happen the first time the NAS uses the key, so the connector must surface it clearly.

## 11. Tests

- **Unit:** deserialization with JSON fixtures copied from the docs, including the `magnet/files` tree
  and live-mode deltas.
- **Integration with `wiremock`:** retries (429 → 200), rate limiter, `wait_until_ready` and a watcher
  sequence (fullsync → delta → ready).
- **Against the real API:** `#[ignore]`d, using `ALLDEBRID_API_KEY` (or the demo key), to confirm the ⚠️ items.

## 12. Known API limits to keep in mind

- At most 30 active magnets (`MAGNET_TOO_MANY_ACTIVE`). The connector should queue.
- `magnet/instant` no longer exists: there is no way to know whether a magnet is cached without uploading it.
  In practice, `ready: true` in the upload response means "it was cached".
- The docs contain mistakes: the PIN example uses v4 while the endpoint is v4.1, the voucher/generate title is wrong,
  the links/save cURL example points to magnet/delete. When in doubt, the real API wins.

## 13. Roadmap

- **v0.1:** this spec.
- **v0.2:** `user`, `user/hosts` (v4.1), `hosts*`, notifications, PIN flow, `link/infos`, `link/streaming`.
- **v0.3:** saved links, history and, if needed, the `blocking` *feature*.

## 14. Verified against the real API (2026-10-22)

Using the real account, a cached magnet (Debian 13.7 netinst ISO, deleted afterwards) and an existing one.

- **`magnet/status` v4.1:** without `id` it returns `magnets` as a list; with `id`, as an **object** (which
  also includes `files` with the `n/s/l/e` tree). Fields seen: `id` (number), `filename`, `size`, `hash`,
  `status`, `statusCode`, `uploadDate`, `completionDate`, `nbLinks`, `notified`, `type` (`"m"`/`"b"`),
  `version`. Ready magnets do **not** include `downloaded`, `seeders`, speeds, etc. (they stay at 0).
- **Live mode:** the first call is `fullsync: true` with everything; then `counter` + only the magnets that
  changed (as full objects, not just the changed fields). Deletion: `{"deleted": true, "id": …}`, repeated
  on later calls. An empty delta has no `fullsync`.
- **`magnet/upload`:** re-uploading the same hash returns the same `id` and `ready: true`. Extra fields:
  `magnet` (echo of the input) and `filename_original`.
- **`magnet/files`:** `id` as a **string**; the tree is as expected.
- **`link/unlock`:** for magnet links it returns the direct `link` immediately (`host: "magnet"`,
  `id` as string, `filename`, `filesize`, `path`, `paws`, `streaming`). The *delayed* case has not been seen.
- **`magnet/restart`:** multiple ids via `ids[]`; the response is `magnets[]` with `magnet` (id as string)
  and a per-item `error`. Restarting a ready magnet → `MAGNET_PROCESSING`; a non-existent id →
  `MAGNET_INVALID_URI` (sic).
- **`magnet/delete`** and **`magnet/status`** on a deleted id → `MAGNET_INVALID_ID`.

Still ⚠️: the meaning of `statusCode` 5–15 and whether `NO_SERVER` is really transient (neither can be
triggered on demand).

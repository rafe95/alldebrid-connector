# Spec: `alldebrid` workspace

This document describes what is implemented. It started as a design template and is kept in
sync with the code; when the two disagree, the code wins and this file should be fixed.

The workspace has two layers:

- **`alldebrid`** (§1–§14): an async Rust library for the AllDebrid API. It is
  context-independent: it knows nothing about indexers, NAS boxes or where files end up.
- **`alldebrid-connector`** (§15): a backend built on the library. It accepts magnets and
  `.torrent` files, sends them to AllDebrid and downloads the finished files into
  per-category folders. An optional static web UI lives in `web/`.

Status: library **v0.1 implemented**, verified against the real API on 2026-10-22 (see §14).
Connector **v0.1 implemented**, run end to end against the real API on 2026-11-12 (see §15.9).
Anything still marked ⚠️ has not been confirmed yet
(demo keys: `staticDemoApikeyPrem`, `staticDemoApikeyTria`, `staticDemoApikeyFree`).

---

## 1. Goals and non-goals

**Goals**
- Cover the flow: *upload magnet → track its status → get its files → get direct links*.
- Strong types for responses and errors (no `serde_json::Value` in the public API).
- Respect the API limits (12 req/s, 600 req/min) and retry transient failures.
- Wait for a magnet to become ready, and watch for changes with the *live* mode of `magnet/status`.

**Non-goals (v0.1)**
- Downloading files to disk (the connector does that; see §8 and §15).
- User/hosts, PIN flow, saved links, history, vouchers. These come later.
- A synchronous/blocking API.

## 2. Repo layout (workspace)

```
alldebrid-connector/          (repo)
├── Cargo.toml                [workspace]
├── SPEC.md
├── README.md                 how to run and configure the connector
├── Dockerfile                multi-arch image of the connector (amd64, arm64)
├── compose.yaml              connector + optional web UI
├── web/                      static web UI (HTML, CSS, JS) and its nginx config
└── crates/
    ├── alldebrid/            library (§1–§14)
    └── alldebrid-connector/  backend: library + binary (§15)
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
| `upload_magnets(&[impl AsRef<str>]) -> Vec<UploadResult>` | `POST /v4/magnet/upload` | URIs or hashes. One `UploadResult { input, result }` per item, where `result` is `Ok(UploadedMagnet)` or `Err(ApiError)`. |
| `upload_magnet(&str) -> UploadedMagnet` | same | single item; a per-item error becomes `Error::Api` |
| `upload_torrent(name, bytes) -> UploadedMagnet` | `POST /v4/magnet/upload/file` | multipart `files[]` |
| `upload_torrents(Vec<(name, bytes)>) -> Vec<UploadResult>` | same | batched |
| `magnet_status(id) -> Magnet` | `POST /v4.1/magnet/status` (`id`) | |
| `magnets(filter: Option<StatusFilter>) -> Vec<Magnet>` | `POST /v4.1/magnet/status` (`status`) | `Active` / `Ready` / `Expired` / `Error` |
| `magnet_files(&[MagnetId]) -> Vec<MagnetFiles>` | `POST /v4/magnet/files` (`id[]`) | tree → see types |
| `magnet_file_tree(id) -> Vec<FileNode>` | same | single magnet |
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

`client.raw::<T: DeserializeOwned>(version: ApiVersion, path: &str, params: &[(&str, &str)]) -> Result<T>`. Works for any endpoint not
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

### 7.3 `unlock_resolved`
`client.unlock_resolved(link, password: Option<&str>, WaitOptions) -> DirectLink { url, filename, size }`:
calls `unlock` and, if the response is *delayed*, polls `delayed` at `poll_interval` until the link is
available (`Error::DelayedLinkFailed` if generation fails, `Error::Timeout` if it takes too long).

## 8. How files reach the NAS (a connector decision, not the library's)

| Option | How | Pros | Cons |
|---|---|---|---|
| **A. API + direct HTTP** (implemented) | `magnet_files` → `unlock_resolved` per file → the connector downloads over HTTPS with `Range` (resumable) | Full control: progress, size verification, filters (e.g. video only, no `sample`), retries, no external dependencies | The downloader has to be written (simple with reqwest) |
| **B. WebDAV + rclone copy** | `https://webdav.debrid.it/`, user = apikey, any non-empty password; `rclone copy alldebrid:magnets/<name> /volume1/…` | Zero download code | Depends on rclone and on how folders are named; weaker error handling and progress reporting |
| **C. Mounted WebDAV** (rclone mount) | Plex/Jellyfin read straight from the mount | Uses no disk space | Not really "downloading"; depends on the network and the rclone cache |

The library provides everything needed for A (B/C need nothing from it beyond the upload).
**Decision:** the connector implements A (see §15.4). B and C are not implemented.

## 9. Main types

```rust
pub struct MagnetId(pub u64);

pub struct UploadedMagnet { id: MagnetId, hash: String, name: String, size: u64, ready: bool }
pub struct UploadResult { input: String, result: Result<UploadedMagnet, ApiError> }

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
    Failed(u8),          // 5..=15 (upload failure, file too big, no peers, …)
    Unknown(u8),
}
impl StatusCode { fn is_ready(&self) -> bool; fn is_failed(&self) -> bool; fn is_terminal(&self) -> bool; }

pub enum FileNode {          // from the n/s/l/e keys of magnet/files
    File { name: String, size: u64, link: String },
    Dir  { name: String, children: Vec<FileNode> },
}
impl FileNode { fn flatten(nodes: &[FileNode]) -> Vec<FlatFile /* path, size, link */>; }

pub struct MagnetFiles { id: MagnetId, result: Result<Vec<FileNode>, ApiError> }
pub struct DirectLink { url: String, filename: String, size: u64 }
pub enum DelayedStatus { Processing { time_left: u64 }, Ready { link: String }, Error }
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
    DelayedLinkFailed,                   // delayed link generation reported an error
    UnexpectedResponse(String),          // well formed, but missing what was expected
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
email). It will probably happen the first time the NAS uses the key. The connector surfaces it as the
item's `error` text (the upload is refused and the item becomes `failed`).

## 11. Tests

- **Unit:** deserialization with JSON fixtures copied from the docs, including the `magnet/files` tree
  and live-mode deltas.
- **Integration with `wiremock`:** retries (429 → 200), rate limiter, `wait_until_ready` and a watcher
  sequence (fullsync → delta → ready).
- **Against the real API:** `#[ignore]`d, using `ALLDEBRID_API_KEY` (or the demo key), to confirm the ⚠️ items.

## 12. Known API limits to keep in mind

- At most 30 active magnets (`MAGNET_TOO_MANY_ACTIVE`). The connector queues (§15.3).
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

---

## 15. Connector (`crates/alldebrid-connector`, v0.1)

A single binary (also usable as a library crate, which the tests do). It runs standalone: nothing
in it depends on Radarr, Sonarr, Prowlarr or the web UI.

### 15.1 Goals and non-goals

**Goals**
- Accept a magnet, a bare info-hash, a `.torrent` file, or an http(s) link to a `.torrent`.
- Let the sender choose a category; each category maps to a folder.
- Send items to AllDebrid, wait, and download the finished files to disk.
- Be usable by Radarr/Sonarr as a download client, and by hand through a REST API or the web UI.
- Run from a container with configuration in environment variables.

**Non-goals (v0.1)**
- Authentication. The service is meant for a trusted LAN; anyone who can reach the port can
  queue downloads on the account.
- Talking to indexers. Prowlarr/Radarr find releases; the connector only receives them.
- WebDAV/rclone modes (§8 B and C), file filters, renaming, or seeding.

### 15.2 Categories

- Defaults: `tv`, `movies`, `adult`, `other`, each a folder under `AD_DOWNLOAD_DIR`.
- `AD_CATEGORY_DIRS` (`name=/path,name=/path`) adds categories or overrides their folders.
- A missing category becomes `other`. An unknown category keeps its label (so clients can still
  filter on it) but its files go to the `other` folder. Lookup is case-insensitive.
- An item's files land in `<category folder>/<item name>/…`. If AllDebrid's file tree already starts
  with a folder named like the item, that level is not duplicated.

### 15.3 Item lifecycle

Items are identified by their lowercase hex v1 info-hash, computed locally (from the magnet's
`xt=urn:btih:` in hex or base32, or by hashing the `info` dictionary of the `.torrent`).

```
queued ──upload──▶ processing ──AllDebrid ready──▶ downloading ──all files on disk──▶ completed
   │                   │                                │
   └───────────────────┴────────────── error ───────────┴──▶ failed
```

- **queued → processing:** a worker loop (every `AD_POLL_SECONDS`, and immediately after an add)
  uploads queued items while fewer than 30 are processing. `MAGNET_TOO_MANY_ACTIVE` leaves the queue
  waiting; other API errors fail the item; network errors are retried on the next tick.
  A queued `.torrent` is kept in `<data dir>/torrents/<hash>.torrent` until it is uploaded.
- **processing → downloading:** one `magnets()` call per tick covers all processing items. A magnet
  with a failed status, or one that vanished from the account, fails the item.
- **downloading → completed:** see §15.4. Afterwards the magnet is deleted from AllDebrid when
  `AD_DELETE_AFTER_DOWNLOAD` is true (default).
- Adding a hash that already exists is a no-op, unless the existing item is `failed`, in which case
  it is replaced and retried.
- Removing an item deletes it from the list and from AllDebrid, stops a download in progress, and
  optionally deletes its folder from disk.

**Persistence:** all items are stored in `<data dir>/state.json`, rewritten atomically on every state
change (not on every progress update). On start, items that were `downloading` go back to
`processing`, so they are re-checked and their downloads resume.

### 15.4 Downloading

- At most `AD_MAX_DOWNLOADS` items download at once; files within an item download one after another.
- Per file: `unlock_resolved` → HTTPS GET into `<file>.part` → size check → rename to the final name.
- A `.part` file is resumed with `Range: bytes=<n>-`. `206` appends, `200` starts over, `416` discards
  the part.
- A file already on disk with the expected size is skipped.
- Each file is tried up to 4 times with a growing pause; after that the item fails with the error text.
- The file list (sanitised relative paths and sizes) is saved with the item before the first file is
  fetched; it drives `content_path` and `torrents/files`.
- Paths from AllDebrid are sanitised: `.`/`..` and empty components are dropped, and separators and
  control characters inside a component are replaced, so nothing can be written outside the item folder.

### 15.5 REST API

| Method and path | Body | Result |
|---|---|---|
| `GET /health` | – | `ok` |
| `GET /api/categories` | – | `{ name: folder }` |
| `GET /api/downloads` | – | list of items, oldest first |
| `GET /api/downloads/{hash}` | – | one item, or 404 |
| `POST /api/downloads` | JSON `{ "magnet": …, "category": … }` (`url` is an alias of `magnet`) | 202 `{ "hash" }`, or 400 `{ "error" }` |
| `POST /api/downloads/torrent` | multipart: `category` plus one or more `.torrent` files | 202 `{ "hashes": [...] }`, or 400 |
| `DELETE /api/downloads/{hash}?delete_files=true` (or `false`) | – | 204, or 404 |

An item is `{ hash, name, category, state, error, progress, size, path, added_on, completed_on }`.
`state` is `queued`, `processing`, `downloading`, `completed` or `failed`. `progress` is 0–1: the
AllDebrid phase is the first half and the download to disk the second.
Request bodies are limited to 32 MB.

### 15.6 qBittorrent Web API emulation (`/api/v2`)

Enough of the qBittorrent API for Radarr/Sonarr to add the connector as a qBittorrent download client.

| Endpoint | Behaviour |
|---|---|
| `auth/login` | Always `Ok.` with a `SID` cookie; any credentials are accepted |
| `app/version`, `app/webapiVersion` | `v4.6.7`, `2.9.3` |
| `app/preferences` | Default save path; ratio and seeding limits disabled |
| `torrents/categories` | Configured categories plus any labels in use |
| `torrents/add` | Multipart or urlencoded: `urls` (one per line), `torrents` (files), `category` |
| `torrents/info` | Optional `category` and `hashes` (separated by a pipe character, or `all`) filters |
| `torrents/properties` | Minimal data for one `hash` |
| `torrents/files` | One entry per file of `hash`; `name` is relative to `save_path` (`<item folder>/<path>`) |
| `torrents/delete` | `hashes` (required) and `deleteFiles` |
| `torrents/setCategory` | Changes the label only; files already on disk are not moved |
| anything else | Accepted and ignored (`Ok.`): pause, resume, share limits, `createCategory`… |

State mapping: `queued` → `queuedDL`; `processing`/`downloading` → `downloading`; `completed` →
`pausedUP` with `ratio` 1.0 against a `ratio_limit` of 1.0, so the client sees a finished torrent that
has met its seeding goal and may import and remove it; `completed` whose files are no longer on
disk (or cannot be read) → `missingFiles`, which Radarr/Sonarr show as a warning; `failed` → `error`.

Paths (since 0.1.1), following qBittorrent: `save_path` is the category folder. `content_path` is
the file itself for a single-file item (`<save_path>/<item folder>/<file>`) and the item folder for a
multi-file item, or while the files are not known yet. For every entry of `torrents/files`,
`<save_path>/<name>` is the file on disk. Every item keeps its own folder on disk whatever the number
of files; only the reported paths follow qBittorrent. Items completed by 0.1.0 have no recorded file
list: at startup it is read back from their folder (`.part` files ignored) and saved.
Radarr/Sonarr must see those paths as the connector does (same mount, or a remote path mapping).

### 15.7 Configuration

Environment variables read by the binary:

| Variable | Default | Meaning |
|---|---|---|
| `ALLDEBRID_API_KEY` | required | AllDebrid API key |
| `ALLDEBRID_BASE_URL` | AllDebrid | API base URL override (tests) |
| `AD_LISTEN` | `0.0.0.0:8080` | Listen address |
| `AD_DATA_DIR` | `/data` | `state.json` and queued `.torrent` files |
| `AD_DOWNLOAD_DIR` | `/downloads` | Parent of the default category folders |
| `AD_CATEGORY_DIRS` | – | Extra or overridden categories |
| `AD_DELETE_AFTER_DOWNLOAD` | `true` | Delete the magnet from AllDebrid once it is on disk |
| `AD_MAX_DOWNLOADS` | `2` | Items downloaded to disk at the same time |
| `AD_POLL_SECONDS` | `5` | AllDebrid polling interval |
| `RUST_LOG` | `info` | Log level |

The binary does not read `.env` files; Compose does.

### 15.8 Packaging and web UI

- **Image:** `Dockerfile` cross-compiles in the build stage, so one build produces `linux/amd64` and
  `linux/arm64` without CPU emulation. Published as `ghcr.io/rafe95/alldebrid-connector:<version>`.
  The image holds only the connector binary, with `/data` and `/downloads` as volumes and port 8080.
- **Compose:** `compose.yaml` runs the connector and, optionally, the `web` service. `API_PORT`
  (default 8080) and `WEB_PORT` (default 8081) set the ports published on the host.
- **Web UI:** a separate, optional service. Stock nginx serves `web/index.html`, `style.css` and
  `app.js` (no framework, no build step) and proxies `/api/` to the connector, so the browser uses a
  single origin. The page adds magnets, links and `.torrent` files with a category, lists items with
  state and progress (refreshed every 3 s), and removes them with or without their files.

### 15.9 Tests and verification

- **Unit:** magnet and `.torrent` hashing, category resolution, path sanitising, state persistence,
  backfilling the file list of 0.1.0 items; `content_path`/`torrents/files` for a single MKV in a
  release-named folder, a single MP4, several videos, videos with subtitles in a subfolder, completed
  and incomplete items, names with spaces, brackets, quotes, backslashes and Unicode (valid JSON
  escapes only), and missing files (`missingFiles`).
- **End to end (`tests/e2e.rs`):** the real server and worker against a `wiremock` AllDebrid and file
  host: qBittorrent add → download → delete, resuming a `.part` file through the REST API, an unknown
  category landing in `other`, rejection of invalid input, a single file named like the release
  (`<name>/<name>`, the 0.1.0 Radarr import failure), and a multi-file item with a subfolder.
- **Real API (2026-11-12):** the Fedora Budgie 45 `.torrent` link, category `other`: fetched,
  uploaded with `magnet/upload/file`, downloaded (3.1 GB), SHA-256 matched the official checksum,
  and the magnet was deleted afterwards without error.
- **Containers (2026-11-12):** the image builds for both architectures, and the compose stack
  (connector + nginx UI) runs under podman.

Still ⚠️: no real Radarr/Sonarr has been connected yet, so §15.6 (in particular the completed-state
mapping) is unverified; no download has been run from inside a container; the web UI's file upload
and remove buttons were not exercised in a browser.

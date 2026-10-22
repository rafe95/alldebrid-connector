# alldebrid

Async Rust client for the [AllDebrid API](https://docs.alldebrid.com/), focused on the
magnet workflow: upload a magnet, follow its progress, list its files and turn them into
direct HTTPS links you can download with any HTTP client.

```rust
use alldebrid::{Client, FileNode, WaitOptions};

let client = Client::new(std::env::var("ALLDEBRID_API_KEY")?)?;

let magnet = client.upload_magnet("magnet:?xt=urn:btih:...").await?;
client.wait_until_ready(magnet.id, WaitOptions::default()).await?;

for file in FileNode::flatten(&client.magnet_file_tree(magnet.id).await?) {
    let direct = client.unlock_resolved(&file.link, None, WaitOptions::default()).await?;
    println!("{} -> {}", file.path, direct.url);
}
```

## Features

| Area | API |
|---|---|
| Magnets | `upload_magnets`, `upload_magnet`, `upload_torrents`, `upload_torrent`, `magnet_status`, `magnets`, `magnet_files`, `magnet_file_tree`, `delete_magnet`, `restart_magnet(s)` |
| Links | `unlock`, `delayed` |
| Live mode | `live_status` (low level), `watch_magnets` (a `Stream` of `MagnetEvent`s) |
| Helpers | `wait_until_ready`, `unlock_resolved`, `FileNode::flatten` |
| Anything else | `raw(version, path, params)` |

### Watching every magnet

```rust
use futures_util::StreamExt;
use alldebrid::{MagnetEvent, WatchOptions};

let mut events = std::pin::pin!(client.watch_magnets(WatchOptions::default()));
while let Some(event) = events.next().await {
    match event? {
        MagnetEvent::Ready(m) => println!("{} is ready", m.filename),
        MagnetEvent::Failed(m) => println!("{} failed: {}", m.filename, m.status),
        _ => {}
    }
}
```

Live mode only transfers what changed since the previous poll.

## Behaviour

- **Rate limiting:** client-side token buckets matching AllDebrid's limits (12 req/s,
  600 req/min), shared by all clones of a `Client`. Configure with `.rate_limit(..)` or
  disable with `.no_rate_limit()`.
- **Retries:** network errors, HTTP 429/502/503/504 and `NO_SERVER` are retried with
  exponential backoff and jitter (`RetryPolicy`). `Retry-After` is honoured. Magnet uploads
  are retried like any other call (re-uploading a hash returns the existing magnet).
  `.torrent` uploads are retried only when the request certainly did not reach the server
  (connection failure, HTTP 429).
- **Errors:** `Error::Api` carries a typed `ApiErrorCode`. Unknown codes are kept as
  `ApiErrorCode::Other`, so new codes never break deserialization.
- **API versions:** each endpoint is pinned to its current version (`/v4/` or `/v4.1/`).
- **TLS:** rustls, so no OpenSSL is needed (handy when cross-compiling for a NAS).

## Testing

```sh
cargo test -p alldebrid                     # unit + mock-server tests
ALLDEBRID_API_KEY=... cargo test -p alldebrid --test live -- --ignored --nocapture
```

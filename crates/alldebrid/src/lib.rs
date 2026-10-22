//! Async client for the [AllDebrid API](https://docs.alldebrid.com/).
//!
//! Covers the magnet workflow end to end: upload a magnet, follow its status
//! (by polling or with live mode), list its files, and unlock them into direct
//! HTTPS links that any HTTP client can download.
//!
//! ```no_run
//! use alldebrid::{Client, FileNode, WaitOptions};
//!
//! # async fn run() -> alldebrid::Result<()> {
//! let client = Client::new(std::env::var("ALLDEBRID_API_KEY").unwrap())?;
//!
//! let magnet = client.upload_magnet("magnet:?xt=urn:btih:...").await?;
//! client.wait_until_ready(magnet.id, WaitOptions::default()).await?;
//!
//! for file in FileNode::flatten(&client.magnet_file_tree(magnet.id).await?) {
//!     let direct = client.unlock_resolved(&file.link, None, WaitOptions::default()).await?;
//!     println!("{} -> {}", file.path, direct.url);
//! }
//! # Ok(()) }
//! ```
//!
//! # Behaviour
//!
//! - Requests are rate limited client-side to AllDebrid's documented limits
//!   (12 req/s, 600 req/min); see [`RateLimit`].
//! - Network errors, HTTP 429/502/503/504 and `NO_SERVER` errors are retried with
//!   exponential backoff; see [`RetryPolicy`]. Uploads are only retried when the
//!   request certainly did not reach the server.
//! - Each endpoint is pinned to its current API version (`v4` or `v4.1`).

#![warn(missing_docs)]

mod api;
mod client;
mod error;
mod helpers;
mod rate_limit;
mod retry;
mod types;

pub use client::{ApiVersion, Client, ClientBuilder};
pub use error::{ApiError, ApiErrorCode, Error, Result};
pub use helpers::wait::WaitOptions;
pub use helpers::watch::{MagnetEvent, WatchOptions};
pub use rate_limit::RateLimit;
pub use retry::RetryPolicy;
pub use types::{
    DelayedStatus, DirectLink, FileNode, FlatFile, LiveSession, LiveUpdate, Magnet, MagnetDelta,
    MagnetFiles, MagnetId, StatusCode, StatusFilter, Unlocked, UploadResult, UploadedMagnet,
};

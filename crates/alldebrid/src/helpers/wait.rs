//! Polling helpers: wait for a magnet, resolve delayed links.

use std::time::Duration;

use tokio::time::Instant;

use crate::error::{Error, Result};
use crate::types::{DelayedStatus, DirectLink, Magnet, MagnetId};
use crate::Client;

/// Options for polling helpers.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitOptions {
    /// Time between polls (default 5 s, as recommended by AllDebrid).
    pub poll_interval: Duration,
    /// Give up after this long. `None` waits forever.
    pub timeout: Option<Duration>,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            timeout: None,
        }
    }
}

impl WaitOptions {
    fn deadline(&self) -> Option<Instant> {
        self.timeout.map(|t| Instant::now() + t)
    }
}

async fn sleep_or_timeout(interval: Duration, deadline: Option<Instant>) -> Result<()> {
    match deadline {
        Some(d) if Instant::now() + interval > d => {
            if Instant::now() >= d {
                return Err(Error::Timeout);
            }
            tokio::time::sleep_until(d).await;
        }
        _ => tokio::time::sleep(interval).await,
    }
    Ok(())
}

impl Client {
    /// Poll a magnet until it is ready.
    ///
    /// Returns the ready magnet, [`Error::MagnetFailed`] if it reaches an error
    /// status, or [`Error::Timeout`].
    pub async fn wait_until_ready(&self, id: MagnetId, opts: WaitOptions) -> Result<Magnet> {
        let deadline = opts.deadline();
        loop {
            let m = self.magnet_status(id).await?;
            if m.status_code.is_ready() {
                return Ok(m);
            }
            if m.status_code.is_failed() {
                return Err(Error::MagnetFailed {
                    id,
                    code: m.status_code,
                    message: m.status,
                });
            }
            tracing::debug!(%id, status = %m.status, downloaded = m.downloaded, size = m.size, "magnet not ready yet");
            sleep_or_timeout(opts.poll_interval, deadline).await?;
        }
    }

    /// Unlock a link and, if generation is delayed, poll until the direct link is ready.
    pub async fn unlock_resolved(
        &self,
        link: &str,
        password: Option<&str>,
        opts: WaitOptions,
    ) -> Result<DirectLink> {
        let u = self.unlock(link, password).await?;
        if !u.link.is_empty() {
            return Ok(DirectLink {
                url: u.link,
                filename: u.filename,
                size: u.filesize,
            });
        }
        let delayed_id = u.delayed.ok_or_else(|| {
            Error::UnexpectedResponse("unlock returned neither a link nor a delayed id".into())
        })?;
        let deadline = opts.deadline();
        loop {
            sleep_or_timeout(opts.poll_interval, deadline).await?;
            match self.delayed(delayed_id).await? {
                DelayedStatus::Ready { link } => {
                    return Ok(DirectLink {
                        url: link,
                        filename: u.filename,
                        size: u.filesize,
                    })
                }
                DelayedStatus::Error => return Err(Error::DelayedLinkFailed),
                DelayedStatus::Processing { time_left } => {
                    tracing::debug!(delayed_id, time_left, "delayed link still processing");
                }
            }
        }
    }
}

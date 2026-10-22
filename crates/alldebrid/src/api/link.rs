//! `link/*` endpoints needed to turn magnet files into direct downloads.

use crate::client::{ApiVersion, Body, Idempotency};
use crate::error::Result;
use crate::types::{DelayedStatus, Unlocked};
use crate::Client;

impl Client {
    /// Unlock a link (e.g. a file link from [`magnet_file_tree`](Client::magnet_file_tree)).
    ///
    /// The result may be delayed; see [`unlock_resolved`](Client::unlock_resolved)
    /// for a helper that waits for the final link.
    pub async fn unlock(&self, link: &str, password: Option<&str>) -> Result<Unlocked> {
        let mut form = vec![("link".to_owned(), link.to_owned())];
        if let Some(p) = password {
            form.push(("password".to_owned(), p.to_owned()));
        }
        self.call(
            ApiVersion::V4,
            "link/unlock",
            Body::Form(form),
            Idempotency::Safe,
        )
        .await
    }

    /// Status of a delayed link.
    pub async fn delayed(&self, id: u64) -> Result<DelayedStatus> {
        let form = vec![("id".to_owned(), id.to_string())];
        self.call(
            ApiVersion::V4,
            "link/delayed",
            Body::Form(form),
            Idempotency::Safe,
        )
        .await
    }
}

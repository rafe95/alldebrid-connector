//! `magnet/*` endpoints.

use serde::Deserialize;

use crate::client::{ApiVersion, Body, Idempotency};
use crate::error::{ApiError, Error, Result};
use crate::types::{
    FileNode, LiveSession, LiveUpdate, Magnet, MagnetDelta, MagnetFiles, MagnetId, RawMagnetFiles,
    RawUploadEntry, StatusFilter, UploadResult,
};
use crate::Client;

/// The API returns either a list or a single object depending on the request.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    Many(Vec<T>),
    One(T),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::Many(v) => v,
            OneOrMany::One(t) => vec![t],
        }
    }
}

fn empty<T>() -> OneOrMany<T> {
    OneOrMany::Many(Vec::new())
}

#[derive(Deserialize)]
struct MagnetsData<T> {
    #[serde(default = "empty")]
    magnets: OneOrMany<T>,
}

#[derive(Deserialize)]
struct FilesData {
    #[serde(default)]
    files: Vec<RawUploadEntry>,
}

#[derive(Deserialize)]
struct LiveData {
    #[serde(default = "empty")]
    magnets: OneOrMany<MagnetDelta>,
    #[serde(default)]
    counter: u64,
    #[serde(default)]
    fullsync: bool,
}

#[derive(Deserialize)]
struct RestartEntry {
    magnet: MagnetId,
    error: Option<ApiError>,
}

fn upload_results(entries: Vec<RawUploadEntry>) -> Result<Vec<UploadResult>> {
    entries
        .into_iter()
        .map(|e| {
            e.into_result().map_err(|source| Error::Decode {
                source,
                body: "upload entry".into(),
            })
        })
        .collect()
}

impl Client {
    /// Add magnets (URIs or info hashes) to the account.
    ///
    /// Returns one result per input, in the same order. A per-item failure
    /// (e.g. `MAGNET_INVALID_URI`) does not fail the whole call.
    pub async fn upload_magnets<S: AsRef<str>>(&self, magnets: &[S]) -> Result<Vec<UploadResult>> {
        let form = magnets
            .iter()
            .map(|m| ("magnets[]".to_owned(), m.as_ref().to_owned()))
            .collect();
        let data: MagnetsData<RawUploadEntry> = self
            .call(
                ApiVersion::V4,
                "magnet/upload",
                Body::Form(form),
                // Re-uploading a hash returns the existing magnet, so retries are safe.
                Idempotency::Safe,
            )
            .await?;
        upload_results(data.magnets.into_vec())
    }

    /// Add a single magnet and return it, failing on a per-item error.
    pub async fn upload_magnet(&self, magnet: &str) -> Result<crate::UploadedMagnet> {
        self.upload_magnets(&[magnet])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::UnexpectedResponse("empty upload response".into()))?
            .result
            .map_err(Error::Api)
    }

    /// Upload `.torrent` files, given as `(file name, contents)`.
    pub async fn upload_torrents(
        &self,
        files: Vec<(String, Vec<u8>)>,
    ) -> Result<Vec<UploadResult>> {
        let parts = files
            .into_iter()
            .map(|(name, bytes)| ("files[]".to_owned(), name, bytes))
            .collect();
        let data: FilesData = self
            .call(
                ApiVersion::V4,
                "magnet/upload/file",
                Body::Multipart(parts),
                Idempotency::Unsafe,
            )
            .await?;
        upload_results(data.files)
    }

    /// Upload a single `.torrent` file.
    pub async fn upload_torrent(
        &self,
        name: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<crate::UploadedMagnet> {
        self.upload_torrents(vec![(name.into(), bytes)])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::UnexpectedResponse("empty upload response".into()))?
            .result
            .map_err(Error::Api)
    }

    /// Status of one magnet (`magnet/status` v4.1).
    pub async fn magnet_status(&self, id: MagnetId) -> Result<Magnet> {
        let form = vec![("id".to_owned(), id.to_string())];
        let data: MagnetsData<Magnet> = self
            .call(
                ApiVersion::V4_1,
                "magnet/status",
                Body::Form(form),
                Idempotency::Safe,
            )
            .await?;
        data.magnets
            .into_vec()
            .into_iter()
            .next()
            .ok_or_else(|| Error::UnexpectedResponse(format!("magnet {id} missing from status")))
    }

    /// All magnets of the account, optionally filtered by status.
    pub async fn magnets(&self, filter: Option<StatusFilter>) -> Result<Vec<Magnet>> {
        let form = filter
            .map(|f| vec![("status".to_owned(), f.as_str().to_owned())])
            .unwrap_or_default();
        let data: MagnetsData<Magnet> = self
            .call(
                ApiVersion::V4_1,
                "magnet/status",
                Body::Form(form),
                Idempotency::Safe,
            )
            .await?;
        Ok(data.magnets.into_vec())
    }

    /// One live-mode poll of `magnet/status`.
    ///
    /// The first call (counter 0) returns every magnet with `fullsync: true`;
    /// following calls only return changed fields. `session.counter` is updated.
    /// Most users want [`watch_magnets`](Client::watch_magnets) instead.
    pub async fn live_status(&self, session: &mut LiveSession) -> Result<LiveUpdate> {
        let form = vec![
            ("session".to_owned(), session.session.to_string()),
            ("counter".to_owned(), session.counter.to_string()),
        ];
        let data: LiveData = self
            .call(
                ApiVersion::V4_1,
                "magnet/status",
                Body::Form(form),
                Idempotency::Safe,
            )
            .await?;
        session.counter = data.counter;
        Ok(LiveUpdate {
            fullsync: data.fullsync,
            magnets: data.magnets.into_vec(),
        })
    }

    /// File trees of several magnets (`magnet/files`).
    pub async fn magnet_files(&self, ids: &[MagnetId]) -> Result<Vec<MagnetFiles>> {
        let form = ids
            .iter()
            .map(|id| ("id[]".to_owned(), id.to_string()))
            .collect();
        let data: MagnetsData<RawMagnetFiles> = self
            .call(
                ApiVersion::V4,
                "magnet/files",
                Body::Form(form),
                Idempotency::Safe,
            )
            .await?;
        Ok(data
            .magnets
            .into_vec()
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// File tree of one magnet. Use [`FileNode::flatten`] to get a flat list.
    pub async fn magnet_file_tree(&self, id: MagnetId) -> Result<Vec<FileNode>> {
        self.magnet_files(&[id])
            .await?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| Error::UnexpectedResponse(format!("magnet {id} missing from files")))?
            .result
            .map_err(Error::Api)
    }

    /// Delete a magnet.
    pub async fn delete_magnet(&self, id: MagnetId) -> Result<()> {
        let form = vec![("id".to_owned(), id.to_string())];
        self.call::<serde_json::Value>(
            ApiVersion::V4,
            "magnet/delete",
            Body::Form(form),
            Idempotency::Safe,
        )
        .await?;
        Ok(())
    }

    /// Restart a failed magnet.
    pub async fn restart_magnet(&self, id: MagnetId) -> Result<()> {
        let form = vec![("id".to_owned(), id.to_string())];
        self.call::<serde_json::Value>(
            ApiVersion::V4,
            "magnet/restart",
            Body::Form(form),
            Idempotency::Safe,
        )
        .await?;
        Ok(())
    }

    /// Restart several failed magnets; returns one result per magnet.
    pub async fn restart_magnets(
        &self,
        ids: &[MagnetId],
    ) -> Result<Vec<(MagnetId, Result<(), ApiError>)>> {
        let form = ids
            .iter()
            .map(|id| ("ids[]".to_owned(), id.to_string()))
            .collect();
        let data: MagnetsData<RestartEntry> = self
            .call(
                ApiVersion::V4,
                "magnet/restart",
                Body::Form(form),
                Idempotency::Safe,
            )
            .await?;
        Ok(data
            .magnets
            .into_vec()
            .into_iter()
            .map(|e| (e.magnet, e.error.map_or(Ok(()), Err)))
            .collect())
    }
}

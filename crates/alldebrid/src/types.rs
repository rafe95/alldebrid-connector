//! Data types returned by the API.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::ApiError;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Identifier of a magnet in the user's AllDebrid account.
///
/// The API sometimes sends ids as numbers and sometimes as strings; both are accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MagnetId(pub u64);

impl fmt::Display for MagnetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<u64> for MagnetId {
    fn from(v: u64) -> Self {
        MagnetId(v)
    }
}

impl<'de> Deserialize<'de> for MagnetId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum NumOrStr {
            Num(u64),
            Str(String),
        }
        match NumOrStr::deserialize(d)? {
            NumOrStr::Num(n) => Ok(MagnetId(n)),
            NumOrStr::Str(s) => s
                .trim()
                .parse()
                .map(MagnetId)
                .map_err(|_| serde::de::Error::custom(format!("invalid magnet id {s:?}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Magnet status
// ---------------------------------------------------------------------------

/// `statusCode` of a magnet.
///
/// Codes 0–3 mean "processing", 4 means "ready" and 5–15 are terminal errors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StatusCode {
    /// 0: queued.
    #[default]
    InQueue,
    /// 1: downloading.
    Downloading,
    /// 2: compressing / moving.
    Compressing,
    /// 3: uploading to AllDebrid storage.
    Uploading,
    /// 4: ready, files can be unlocked.
    Ready,
    /// 5–15: terminal failure. The raw code is kept; see AllDebrid docs for meanings
    /// (e.g. upload failed, file too big, no peers, took too long).
    Failed(u8),
    /// A code outside the documented 0–15 range.
    Unknown(u8),
}

impl StatusCode {
    /// Build from the raw numeric code.
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => Self::InQueue,
            1 => Self::Downloading,
            2 => Self::Compressing,
            3 => Self::Uploading,
            4 => Self::Ready,
            5..=15 => Self::Failed(code),
            other => Self::Unknown(other),
        }
    }

    /// The raw numeric code.
    pub fn code(&self) -> u8 {
        match *self {
            Self::InQueue => 0,
            Self::Downloading => 1,
            Self::Compressing => 2,
            Self::Uploading => 3,
            Self::Ready => 4,
            Self::Failed(c) | Self::Unknown(c) => c,
        }
    }

    /// `true` when the magnet is ready.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// `true` when the magnet failed.
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// `true` when the status will not change any more (ready or failed).
    pub fn is_terminal(&self) -> bool {
        self.is_ready() || self.is_failed()
    }
}

impl<'de> Deserialize<'de> for StatusCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from_code(u8::deserialize(d)?))
    }
}

/// Filter for [`Client::magnets`](crate::Client::magnets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    /// Magnets still being processed.
    Active,
    /// Magnets ready to unlock.
    Ready,
    /// Magnets that expired.
    Expired,
    /// Magnets in error.
    Error,
}

impl StatusFilter {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Ready => "ready",
            Self::Expired => "expired",
            Self::Error => "error",
        }
    }
}

/// A magnet as returned by `magnet/status` (v4.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct Magnet {
    /// Magnet id.
    pub id: MagnetId,
    /// Torrent name.
    pub filename: String,
    /// Total size in bytes.
    pub size: u64,
    /// Human readable status, e.g. `"Downloading"`.
    pub status: String,
    /// Machine readable status.
    pub status_code: StatusCode,
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Bytes uploaded to AllDebrid storage so far.
    pub uploaded: u64,
    /// Number of seeders.
    pub seeders: u32,
    /// Download speed in bytes/s.
    pub download_speed: u64,
    /// Upload speed in bytes/s.
    pub upload_speed: u64,
    /// Unix timestamp of the upload.
    pub upload_date: i64,
    /// Unix timestamp of completion, `None` while not completed.
    #[serde(deserialize_with = "zero_as_none")]
    pub completion_date: Option<i64>,
}

fn zero_as_none<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<i64>::deserialize(d)?.filter(|v| *v != 0))
}

/// Partial magnet sent by `magnet/status` in live mode: only `id` is guaranteed,
/// other fields are present only when they changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct MagnetDelta {
    /// Magnet id.
    pub id: MagnetId,
    /// See [`Magnet::filename`].
    pub filename: Option<String>,
    /// See [`Magnet::size`].
    pub size: Option<u64>,
    /// See [`Magnet::status`].
    pub status: Option<String>,
    /// See [`Magnet::status_code`].
    pub status_code: Option<StatusCode>,
    /// See [`Magnet::downloaded`].
    pub downloaded: Option<u64>,
    /// See [`Magnet::uploaded`].
    pub uploaded: Option<u64>,
    /// See [`Magnet::seeders`].
    pub seeders: Option<u32>,
    /// See [`Magnet::download_speed`].
    pub download_speed: Option<u64>,
    /// See [`Magnet::upload_speed`].
    pub upload_speed: Option<u64>,
    /// See [`Magnet::upload_date`].
    pub upload_date: Option<i64>,
    /// See [`Magnet::completion_date`].
    pub completion_date: Option<i64>,
    /// The magnet was deleted. Sent as `{"deleted": true, "id": …}`, possibly on
    /// several consecutive polls.
    #[serde(default)]
    pub deleted: bool,
}

impl MagnetDelta {
    /// Apply the changed fields onto `m`.
    pub fn apply_to(&self, m: &mut Magnet) {
        macro_rules! set {
            ($($f:ident),*) => { $( if let Some(v) = &self.$f { m.$f = v.clone(); } )* };
        }
        set!(
            filename,
            size,
            status,
            status_code,
            downloaded,
            uploaded,
            seeders,
            download_speed,
            upload_speed,
            upload_date
        );
        if let Some(c) = self.completion_date {
            m.completion_date = (c != 0).then_some(c);
        }
    }

    /// Build a full magnet, using defaults for missing fields.
    pub fn to_magnet(&self) -> Magnet {
        let mut m = Magnet {
            id: self.id,
            ..Magnet::default()
        };
        self.apply_to(&mut m);
        m
    }
}

/// Live-mode session state for [`Client::live_status`](crate::Client::live_status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    /// Random session id chosen by the client.
    pub session: u32,
    /// Counter returned by the previous call (0 forces a full sync).
    pub counter: u64,
}

impl LiveSession {
    /// New session with a random id.
    pub fn new() -> Self {
        Self {
            session: fastrand::u32(1..),
            counter: 0,
        }
    }
}

impl Default for LiveSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of one live-mode call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveUpdate {
    /// `true` if `magnets` is the full list (first call or resync); `false`
    /// if it only contains changes since the previous call.
    pub fullsync: bool,
    /// Full magnets (when `fullsync`) or deltas.
    pub magnets: Vec<MagnetDelta>,
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

/// A magnet successfully added to the account.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct UploadedMagnet {
    /// Magnet id, used by every other magnet endpoint.
    pub id: MagnetId,
    /// Info hash.
    #[serde(default)]
    pub hash: String,
    /// Torrent name.
    #[serde(default)]
    pub name: String,
    /// Total size in bytes.
    #[serde(default)]
    pub size: u64,
    /// `true` if AllDebrid already had it cached (instantly available).
    #[serde(default)]
    pub ready: bool,
}

/// Outcome for one magnet / torrent of an upload request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadResult {
    /// The magnet URI, hash or torrent file name as echoed back by the API.
    pub input: String,
    /// The uploaded magnet, or the per-item error.
    pub result: Result<UploadedMagnet, ApiError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawUploadEntry {
    #[serde(alias = "file", default)]
    magnet: String,
    error: Option<ApiError>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

impl RawUploadEntry {
    pub(crate) fn into_result(self) -> Result<UploadResult, serde_json::Error> {
        let result = match self.error {
            Some(e) => Err(e),
            None => Ok(serde_json::from_value(serde_json::Value::Object(
                self.rest,
            ))?),
        };
        Ok(UploadResult {
            input: self.magnet,
            result,
        })
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// A node of a magnet's file tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileNode {
    /// A file with its (locked) link, to be passed to `link/unlock`.
    File {
        /// File name.
        name: String,
        /// Size in bytes.
        size: u64,
        /// Locked AllDebrid link.
        link: String,
    },
    /// A directory.
    Dir {
        /// Directory name.
        name: String,
        /// Children.
        children: Vec<FileNode>,
    },
}

impl<'de> Deserialize<'de> for FileNode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            n: String,
            #[serde(default)]
            s: u64,
            l: Option<String>,
            e: Option<Vec<FileNode>>,
        }
        let r = Raw::deserialize(d)?;
        Ok(match r.e {
            Some(children) => FileNode::Dir {
                name: r.n,
                children,
            },
            None => FileNode::File {
                name: r.n,
                size: r.s,
                link: r.l.unwrap_or_default(),
            },
        })
    }
}

/// A file from a magnet's tree, with its full path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatFile {
    /// Path inside the torrent, `/`-separated (e.g. `Season 1/ep01.mkv`).
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Locked AllDebrid link, to be passed to `link/unlock`.
    pub link: String,
}

impl FileNode {
    /// Flatten a tree into a list of files with their full paths.
    pub fn flatten(nodes: &[FileNode]) -> Vec<FlatFile> {
        fn walk(nodes: &[FileNode], prefix: &str, out: &mut Vec<FlatFile>) {
            for n in nodes {
                match n {
                    FileNode::File { name, size, link } => out.push(FlatFile {
                        path: join(prefix, name),
                        size: *size,
                        link: link.clone(),
                    }),
                    FileNode::Dir { name, children } => walk(children, &join(prefix, name), out),
                }
            }
        }
        fn join(prefix: &str, name: &str) -> String {
            if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            }
        }
        let mut out = Vec::new();
        walk(nodes, "", &mut out);
        out
    }
}

/// Files of one magnet, or the per-magnet error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagnetFiles {
    /// Magnet id.
    pub id: MagnetId,
    /// File tree or error.
    pub result: Result<Vec<FileNode>, ApiError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawMagnetFiles {
    pub(crate) id: MagnetId,
    #[serde(default)]
    pub(crate) files: Vec<FileNode>,
    pub(crate) error: Option<ApiError>,
}

impl From<RawMagnetFiles> for MagnetFiles {
    fn from(r: RawMagnetFiles) -> Self {
        MagnetFiles {
            id: r.id,
            result: match r.error {
                Some(e) => Err(e),
                None => Ok(r.files),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// Response of `link/unlock`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct Unlocked {
    /// Direct download link. Empty when generation is delayed.
    pub link: String,
    /// File name.
    pub filename: String,
    /// Host name, e.g. `"magnet"`.
    pub host: String,
    /// Host domain.
    pub host_domain: Option<String>,
    /// File size in bytes.
    pub filesize: u64,
    /// Generation id (used by `link/streaming`).
    pub id: String,
    /// Delayed id: when set, poll [`Client::delayed`](crate::Client::delayed) for the link.
    pub delayed: Option<u64>,
}

/// State of a delayed link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelayedStatus {
    /// Still generating; `time_left` is the estimate in seconds.
    Processing {
        /// Estimated seconds left.
        time_left: u64,
    },
    /// The direct link is ready.
    Ready {
        /// Direct download link.
        link: String,
    },
    /// Generation failed.
    Error,
}

impl<'de> Deserialize<'de> for DelayedStatus {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            status: u8,
            #[serde(default)]
            time_left: u64,
            #[serde(default)]
            link: String,
        }
        let r = Raw::deserialize(d)?;
        Ok(match r.status {
            2 => DelayedStatus::Ready { link: r.link },
            3 => DelayedStatus::Error,
            _ => DelayedStatus::Processing {
                time_left: r.time_left,
            },
        })
    }
}

/// A resolved, directly downloadable link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectLink {
    /// HTTPS URL that can be downloaded with any HTTP client (supports `Range`).
    pub url: String,
    /// File name.
    pub filename: String,
    /// File size in bytes.
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_id_accepts_number_and_string() {
        let a: MagnetId = serde_json::from_str("42").unwrap();
        let b: MagnetId = serde_json::from_str("\"42\"").unwrap();
        assert_eq!(a, b);
        assert!(serde_json::from_str::<MagnetId>("\"x\"").is_err());
    }

    #[test]
    fn status_codes() {
        assert!(StatusCode::from_code(4).is_ready());
        assert!(StatusCode::from_code(15).is_failed());
        assert!(!StatusCode::from_code(1).is_terminal());
        assert_eq!(StatusCode::from_code(99), StatusCode::Unknown(99));
        assert_eq!(StatusCode::from_code(7).code(), 7);
    }

    #[test]
    fn magnet_deserializes() {
        let m: Magnet = serde_json::from_str(
            r#"{"id":1,"filename":"x","size":10,"status":"Ready","statusCode":4,
                "downloaded":10,"uploaded":10,"seeders":0,"downloadSpeed":0,
                "uploadSpeed":0,"uploadDate":1700000000,"completionDate":0,"extra":true}"#,
        )
        .unwrap();
        assert_eq!(m.status_code, StatusCode::Ready);
        assert_eq!(m.completion_date, None);
    }

    #[test]
    fn delta_applies_only_present_fields() {
        let mut m = Magnet {
            id: MagnetId(1),
            filename: "a".into(),
            downloaded: 5,
            ..Default::default()
        };
        let d: MagnetDelta =
            serde_json::from_str(r#"{"id":1,"downloaded":9,"statusCode":4}"#).unwrap();
        d.apply_to(&mut m);
        assert_eq!(m.filename, "a");
        assert_eq!(m.downloaded, 9);
        assert!(m.status_code.is_ready());
    }

    #[test]
    fn file_tree_flattens() {
        let nodes: Vec<FileNode> = serde_json::from_str(
            r#"[{"n":"Show","e":[{"n":"S01","e":[{"n":"e1.mkv","s":100,"l":"https://alldebrid.com/f/a"}]},
                                 {"n":"readme.txt","s":1,"l":"https://alldebrid.com/f/b"}]},
                {"n":"top.nfo","s":2,"l":"https://alldebrid.com/f/c"}]"#,
        )
        .unwrap();
        let flat = FileNode::flatten(&nodes);
        let paths: Vec<_> = flat.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["Show/S01/e1.mkv", "Show/readme.txt", "top.nfo"]);
        assert_eq!(flat[0].size, 100);
    }

    #[test]
    fn upload_entry_success_and_error() {
        let ok: RawUploadEntry = serde_json::from_str(
            r#"{"magnet":"abc","hash":"abc","name":"n","size":1,"ready":true,"id":7}"#,
        )
        .unwrap();
        let ok = ok.into_result().unwrap();
        assert_eq!(ok.result.unwrap().id, MagnetId(7));

        let err: RawUploadEntry = serde_json::from_str(
            r#"{"magnet":"bad","error":{"code":"MAGNET_INVALID_URI","message":"nope"}}"#,
        )
        .unwrap();
        let err = err.into_result().unwrap();
        assert_eq!(err.input, "bad");
        assert!(err.result.is_err());
    }

    #[test]
    fn delayed_status() {
        let s: DelayedStatus = serde_json::from_str(r#"{"status":1,"time_left":30}"#).unwrap();
        assert_eq!(s, DelayedStatus::Processing { time_left: 30 });
        let s: DelayedStatus = serde_json::from_str(r#"{"status":2,"link":"https://x"}"#).unwrap();
        assert_eq!(
            s,
            DelayedStatus::Ready {
                link: "https://x".into()
            }
        );
    }
}

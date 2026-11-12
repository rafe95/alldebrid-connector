//! Download items and their persistent state (a JSON file in the data dir).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Make one path component safe to create on disk.
pub fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    match cleaned {
        "" | "." | ".." => "_".to_owned(),
        _ => cleaned.to_owned(),
    }
}

/// Turn a `/`-separated path from AllDebrid into a safe relative path.
pub fn sanitize_rel_path(p: &str) -> PathBuf {
    p.split('/')
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(sanitize_component)
        .collect()
}

/// Where an item is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    /// Waiting to be sent to AllDebrid.
    Queued,
    /// AllDebrid is fetching the torrent.
    Processing,
    /// Files are being downloaded to disk.
    Downloading,
    /// Everything is on disk.
    Completed,
    /// Gave up; see `error`.
    Failed,
}

/// What was submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A magnet URI.
    Magnet(String),
    /// A `.torrent` file stored in the data dir until it is uploaded.
    Torrent(PathBuf),
}

/// One download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    /// Lowercase hex info-hash; the item's identity.
    pub hash: String,
    /// Display name (also the folder name on disk).
    pub name: String,
    /// Category label as given by the sender.
    pub category: String,
    /// Destination folder of the category.
    pub save_path: PathBuf,
    /// What was submitted.
    pub source: Source,
    /// Current state.
    pub state: ItemState,
    /// Why it failed.
    pub error: Option<String>,
    /// Id of the magnet in the AllDebrid account, once uploaded.
    pub magnet_id: Option<u64>,
    /// Total size in bytes (0 while unknown).
    pub size: u64,
    /// Bytes AllDebrid has fetched so far.
    pub cloud_downloaded: u64,
    /// AllDebrid's fetch speed, bytes/s.
    pub cloud_speed: u64,
    /// Bytes written to disk so far.
    #[serde(default)]
    pub local_downloaded: u64,
    /// Disk download speed, bytes/s.
    #[serde(skip)]
    pub local_speed: u64,
    #[serde(skip)]
    sample: Option<(Instant, u64)>,
    /// When it was added (Unix seconds).
    pub added_on: i64,
    /// When it finished (Unix seconds).
    pub completed_on: Option<i64>,
}

impl Item {
    /// A freshly queued item.
    pub fn new(
        hash: String,
        name: String,
        category: String,
        save_path: PathBuf,
        source: Source,
    ) -> Self {
        Self {
            hash,
            name,
            category,
            save_path,
            source,
            state: ItemState::Queued,
            error: None,
            magnet_id: None,
            size: 0,
            cloud_downloaded: 0,
            cloud_speed: 0,
            local_downloaded: 0,
            local_speed: 0,
            sample: None,
            added_on: now(),
            completed_on: None,
        }
    }

    /// Folder that holds this item's files.
    pub fn content_path(&self) -> PathBuf {
        self.save_path.join(sanitize_component(&self.name))
    }

    /// Overall progress, 0.0..=1.0: the cloud phase is the first half, the disk phase the second.
    pub fn progress(&self) -> f64 {
        let frac = |done: u64| {
            if self.size == 0 {
                0.0
            } else {
                (done as f64 / self.size as f64).min(1.0)
            }
        };
        match self.state {
            ItemState::Queued | ItemState::Failed => 0.0,
            ItemState::Processing => frac(self.cloud_downloaded) * 0.5,
            ItemState::Downloading => 0.5 + frac(self.local_downloaded) * 0.5,
            ItemState::Completed => 1.0,
        }
    }
}

/// All items, saved to disk on every state change.
pub struct Store {
    path: PathBuf,
    items: Mutex<BTreeMap<String, Item>>,
}

impl Store {
    /// Load the state file (or start empty). Interrupted downloads go back to
    /// `Processing`, so the worker re-checks them and resumes.
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
        let path = data_dir.join("state.json");
        let mut items: BTreeMap<String, Item> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        for item in items.values_mut() {
            if item.state == ItemState::Downloading {
                item.state = ItemState::Processing;
            }
        }
        Ok(Self {
            path,
            items: Mutex::new(items),
        })
    }

    fn save(&self, items: &BTreeMap<String, Item>) {
        let tmp = self.path.with_extension("json.tmp");
        let res = serde_json::to_vec_pretty(items)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = res {
            tracing::error!(path = %self.path.display(), "could not save state: {e}");
        }
    }

    /// Add an item. An existing item with the same hash is kept, unless it had failed.
    /// Returns `true` if the item was inserted.
    pub fn insert(&self, item: Item) -> bool {
        let mut items = self.items.lock().unwrap();
        if items
            .get(&item.hash)
            .is_some_and(|old| old.state != ItemState::Failed)
        {
            return false;
        }
        items.insert(item.hash.clone(), item);
        self.save(&items);
        true
    }

    /// A copy of one item.
    pub fn get(&self, hash: &str) -> Option<Item> {
        self.items.lock().unwrap().get(hash).cloned()
    }

    /// Copies of all items, oldest first.
    pub fn list(&self) -> Vec<Item> {
        let mut v: Vec<Item> = self.items.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|i| i.added_on);
        v
    }

    /// Change an item and save. Returns `false` if it no longer exists.
    pub fn update(&self, hash: &str, f: impl FnOnce(&mut Item)) -> bool {
        let mut items = self.items.lock().unwrap();
        let Some(item) = items.get_mut(hash) else {
            return false;
        };
        f(item);
        self.save(&items);
        true
    }

    /// Record disk progress without saving. Returns `false` if the item was removed
    /// (the download should stop).
    pub fn set_progress(&self, hash: &str, bytes: u64) -> bool {
        let mut items = self.items.lock().unwrap();
        let Some(item) = items.get_mut(hash) else {
            return false;
        };
        item.local_downloaded = bytes;
        let now = Instant::now();
        match item.sample {
            Some((t, b)) => {
                let dt = now.duration_since(t).as_secs_f64();
                if dt >= 1.0 {
                    item.local_speed = (bytes.saturating_sub(b) as f64 / dt) as u64;
                    item.sample = Some((now, bytes));
                }
            }
            None => item.sample = Some((now, bytes)),
        }
        true
    }

    /// Remove an item and return it.
    pub fn remove(&self, hash: &str) -> Option<Item> {
        let mut items = self.items.lock().unwrap();
        let item = items.remove(hash);
        if item.is_some() {
            self.save(&items);
        }
        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_paths() {
        assert_eq!(sanitize_component("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(
            sanitize_rel_path("../x/./y//z.mkv"),
            PathBuf::from("x/y/z.mkv")
        );
        assert_eq!(
            sanitize_rel_path("/etc/passwd"),
            PathBuf::from("etc/passwd")
        );
    }

    #[test]
    fn persists_and_resets_interrupted_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let item = |h: &str| {
            Item::new(
                h.into(),
                "n".into(),
                "tv".into(),
                "/d/tv".into(),
                Source::Magnet("m".into()),
            )
        };
        {
            let s = Store::open(dir.path()).unwrap();
            assert!(s.insert(item("a")));
            assert!(!s.insert(item("a")), "duplicates are ignored");
            s.update("a", |i| i.state = ItemState::Downloading);
            s.insert(item("b"));
            s.update("b", |i| i.state = ItemState::Failed);
            assert!(s.insert(item("b")), "failed items can be re-added");
        }
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.get("a").unwrap().state, ItemState::Processing);
        assert_eq!(s.get("b").unwrap().state, ItemState::Queued);
        assert!(s.remove("a").is_some());
        assert!(!s.set_progress("a", 1));
    }
}

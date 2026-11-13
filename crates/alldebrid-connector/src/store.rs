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

/// List the finished files below `root` (recursively, sorted, `.part` files skipped).
/// A missing or unreadable folder gives an empty list.
pub fn scan_files(root: &Path) -> Vec<ItemFile> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<ItemFile>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                walk(root, &path, out);
            } else if meta.is_file() && path.extension().map_or(true, |e| e != "part") {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(ItemFile {
                        path: rel.to_owned(),
                        size: meta.len(),
                    });
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
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

/// One file of a download, relative to the item folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemFile {
    /// Sanitized path below the item folder.
    pub path: PathBuf,
    /// Size in bytes.
    pub size: u64,
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
    /// Files of the item, in download order (empty until AllDebrid has listed them).
    #[serde(default)]
    pub files: Vec<ItemFile>,
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
            files: Vec::new(),
            cloud_downloaded: 0,
            cloud_speed: 0,
            local_downloaded: 0,
            local_speed: 0,
            sample: None,
            added_on: now(),
            completed_on: None,
        }
    }

    /// Name of the item folder below `save_path`.
    pub fn folder_name(&self) -> String {
        sanitize_component(&self.name)
    }

    /// Folder that holds this item's files (one per item, whatever the number of files).
    pub fn folder(&self) -> PathBuf {
        self.save_path.join(self.folder_name())
    }

    /// The qBittorrent `content_path`: the file itself for a single-file item, the item
    /// folder otherwise (and while the files are not known yet).
    pub fn content_path(&self) -> PathBuf {
        match self.files.as_slice() {
            [only] => self.folder().join(&only.path),
            _ => self.folder(),
        }
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
        let mut backfilled = false;
        for item in items.values_mut() {
            if item.state == ItemState::Downloading {
                item.state = ItemState::Processing;
            }
            // Items completed before files were recorded (0.1.0): read them from disk.
            if item.state == ItemState::Completed && item.files.is_empty() {
                item.files = scan_files(&item.folder());
                backfilled |= !item.files.is_empty();
            }
        }
        let store = Self {
            path,
            items: Mutex::new(items),
        };
        if backfilled {
            store.save(&store.items.lock().unwrap());
        }
        Ok(store)
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

    fn item(name: &str, save: &Path, files: &[(&str, u64)]) -> Item {
        let mut i = Item::new(
            "h".into(),
            name.into(),
            "movies".into(),
            save.to_owned(),
            Source::Magnet("m".into()),
        );
        i.files = files
            .iter()
            .map(|(p, s)| ItemFile {
                path: p.into(),
                size: *s,
            })
            .collect();
        i
    }

    #[test]
    fn content_path_is_the_file_for_one_file_and_the_folder_otherwise() {
        let save = Path::new("/downloads/movies");
        let grinch = "How.the.Grinch.Stole.Christmas.2000.2160p.UHD.Blu-ray.Remux.DV.HDR.HEVC.TrueHD.Atmos.7.1-CiNEPHiLES.mkv";
        let i = item(grinch, save, &[(grinch, 5)]);
        assert_eq!(i.content_path(), save.join(grinch).join(grinch));
        assert_eq!(i.folder(), save.join(grinch));

        let i = item("Movie (2001)", save, &[("movie.mp4", 5)]);
        assert_eq!(i.content_path(), save.join("Movie (2001)/movie.mp4"));

        let i = item("Pack", save, &[("a.mkv", 1), ("Subs/a.en.srt", 1)]);
        assert_eq!(i.content_path(), save.join("Pack"));

        let i = item("Unknown.Yet", save, &[]);
        assert_eq!(i.content_path(), save.join("Unknown.Yet"));
    }

    #[test]
    fn completed_items_without_files_are_backfilled_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("downloads/movies");
        let name = "Old.Movie.mkv";
        std::fs::create_dir_all(save.join(name)).unwrap();
        std::fs::write(save.join(name).join(name), "12345").unwrap();
        std::fs::write(save.join(name).join("leftover.mkv.part"), "x").unwrap();
        let mut done = item(name, &save, &[]);
        done.state = ItemState::Completed;
        let mut gone = item("Gone", &save, &[]);
        gone.hash = "g".into();
        gone.state = ItemState::Completed;
        let state: BTreeMap<_, _> = [("h".to_owned(), done), ("g".to_owned(), gone)].into();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        // The 0.1.0 state file has no `files` field.
        let mut json = serde_json::to_value(&state).unwrap();
        for v in json.as_object_mut().unwrap().values_mut() {
            v.as_object_mut().unwrap().remove("files");
        }
        std::fs::write(data.join("state.json"), json.to_string()).unwrap();

        let s = Store::open(&data).unwrap();
        let i = s.get("h").unwrap();
        assert_eq!(
            i.files,
            vec![ItemFile {
                path: name.into(),
                size: 5
            }]
        );
        assert_eq!(i.content_path(), save.join(name).join(name));
        assert!(s.get("g").unwrap().files.is_empty());
        // The backfill is saved.
        assert!(std::fs::read_to_string(data.join("state.json"))
            .unwrap()
            .contains("\"files\""));
    }

    #[test]
    fn scans_nested_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Subs")).unwrap();
        std::fs::write(dir.path().join("b.mkv"), "bb").unwrap();
        std::fs::write(dir.path().join("Subs/a.srt"), "a").unwrap();
        let files = scan_files(dir.path());
        let paths: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths, [PathBuf::from("Subs/a.srt"), PathBuf::from("b.mkv")]);
        assert!(scan_files(&dir.path().join("nope")).is_empty());
    }
}

//! Configuration, read from environment variables.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Category used when none (or an unknown one) is given.
pub const FALLBACK_CATEGORY: &str = "other";
const DEFAULT_CATEGORIES: [&str; 4] = ["tv", "movies", "adult", FALLBACK_CATEGORY];

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// AllDebrid API key.
    pub api_key: String,
    /// Override of the AllDebrid API base URL (tests).
    pub base_url: Option<String>,
    /// Address the HTTP server listens on.
    pub listen: SocketAddr,
    /// Where the state file and queued `.torrent` files live.
    pub data_dir: PathBuf,
    /// Category name -> destination folder. Always contains `other`.
    pub categories: BTreeMap<String, PathBuf>,
    /// Remove the magnet from AllDebrid once it is downloaded.
    pub delete_after_download: bool,
    /// How many items are downloaded to disk at the same time.
    pub max_downloads: usize,
    /// How often AllDebrid is polled.
    pub poll_interval: Duration,
}

impl Config {
    /// Defaults for everything except the API key, with categories under `download_dir`.
    pub fn new(api_key: impl Into<String>, data_dir: &Path, download_dir: &Path) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: None,
            listen: ([0, 0, 0, 0], 8080).into(),
            data_dir: data_dir.to_owned(),
            categories: DEFAULT_CATEGORIES
                .iter()
                .map(|c| ((*c).to_owned(), download_dir.join(c)))
                .collect(),
            delete_after_download: true,
            max_downloads: 2,
            poll_interval: Duration::from_secs(5),
        }
    }

    /// Read the configuration from the environment.
    pub fn from_env() -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let api_key =
            var("ALLDEBRID_API_KEY").ok_or_else(|| anyhow!("ALLDEBRID_API_KEY is not set"))?;
        let data_dir = PathBuf::from(var("AD_DATA_DIR").unwrap_or_else(|| "/data".into()));
        let download_dir =
            PathBuf::from(var("AD_DOWNLOAD_DIR").unwrap_or_else(|| "/downloads".into()));
        let mut cfg = Self::new(api_key, &data_dir, &download_dir);
        cfg.base_url = var("ALLDEBRID_BASE_URL");
        if let Some(v) = var("AD_LISTEN") {
            cfg.listen = v.parse().context("AD_LISTEN must look like 0.0.0.0:8080")?;
        }
        if let Some(v) = var("AD_CATEGORY_DIRS") {
            cfg.add_categories(&v)?;
        }
        if let Some(v) = var("AD_DELETE_AFTER_DOWNLOAD") {
            cfg.delete_after_download =
                matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Some(v) = var("AD_MAX_DOWNLOADS") {
            cfg.max_downloads = v
                .parse::<usize>()
                .context("AD_MAX_DOWNLOADS must be a number")?
                .max(1);
        }
        if let Some(v) = var("AD_POLL_SECONDS") {
            cfg.poll_interval = Duration::from_secs(
                v.parse::<u64>()
                    .context("AD_POLL_SECONDS must be a number")?
                    .max(1),
            );
        }
        Ok(cfg)
    }

    /// Parse `name=/path,name2=/path2` and add or override categories.
    pub fn add_categories(&mut self, spec: &str) -> Result<()> {
        for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (name, path) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("bad category entry {pair:?}, expected name=/path"))?;
            self.categories
                .insert(name.trim().to_ascii_lowercase(), PathBuf::from(path.trim()));
        }
        Ok(())
    }

    /// Folder for a category label; unknown labels go to the `other` folder.
    pub fn dir_for(&self, category: &str) -> &Path {
        self.categories
            .get(&category.to_ascii_lowercase())
            .unwrap_or_else(|| &self.categories[FALLBACK_CATEGORY])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_resolve_with_fallback() {
        let mut cfg = Config::new("k", Path::new("/data"), Path::new("/downloads"));
        assert_eq!(cfg.dir_for("TV"), Path::new("/downloads/tv"));
        assert_eq!(cfg.dir_for("sonarr"), Path::new("/downloads/other"));
        cfg.add_categories("sonarr=/media/tv, other=/misc").unwrap();
        assert_eq!(cfg.dir_for("sonarr"), Path::new("/media/tv"));
        assert_eq!(cfg.dir_for("whatever"), Path::new("/misc"));
        assert!(cfg.add_categories("broken").is_err());
    }
}

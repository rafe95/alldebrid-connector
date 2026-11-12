//! Backend that takes torrents and magnets, sends them to AllDebrid and downloads the
//! finished files into per-category folders. Speaks a small REST API of its own and
//! enough of the qBittorrent Web API for Prowlarr, Sonarr and Radarr to use it as a
//! download client.

pub mod config;
pub mod hash;
pub mod http;
pub mod store;
pub mod worker;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{Notify, Semaphore};

pub use config::Config;
use store::{Item, Source, Store};

/// Shared application state (cheap to clone).
#[derive(Clone)]
pub struct App {
    /// Configuration.
    pub cfg: Arc<Config>,
    /// Items and their state.
    pub store: Arc<Store>,
    /// AllDebrid API client.
    pub client: alldebrid::Client,
    /// Plain HTTP client for file downloads.
    pub http: reqwest::Client,
    /// Limits simultaneous downloads to disk.
    pub slots: Arc<Semaphore>,
    /// Wakes the worker when something is added.
    pub wake: Arc<Notify>,
}

impl App {
    /// Build the application state from a configuration.
    pub fn new(cfg: Config) -> Result<Self> {
        let store = Store::open(&cfg.data_dir)?;
        let mut builder = alldebrid::Client::builder().api_key(cfg.api_key.clone());
        if let Some(url) = &cfg.base_url {
            builder = builder.base_url(url.clone());
        }
        let client = builder.build().context("building the AllDebrid client")?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            slots: Arc::new(Semaphore::new(cfg.max_downloads)),
            cfg: Arc::new(cfg),
            store: Arc::new(store),
            client,
            http,
            wake: Arc::new(Notify::new()),
        })
    }

    fn enqueue(&self, meta: hash::TorrentMeta, category: &str, source: Source) -> String {
        let category = match category.trim() {
            "" => config::FALLBACK_CATEGORY.to_owned(),
            c => c.to_owned(),
        };
        let name = meta.name.unwrap_or_else(|| meta.hash.clone());
        let save_path = self.cfg.dir_for(&category).to_owned();
        let item = Item::new(meta.hash.clone(), name, category, save_path, source);
        if self.store.insert(item) {
            tracing::info!(hash = %meta.hash, "queued");
            self.wake.notify_one();
        }
        meta.hash
    }

    /// Queue a magnet URI (or bare info-hash). Returns the info-hash.
    pub fn add_magnet(&self, uri: &str, category: &str) -> Result<String> {
        let meta = hash::parse_magnet(uri)?;
        Ok(self.enqueue(meta, category, Source::Magnet(uri.trim().to_owned())))
    }

    /// Queue a `.torrent` file. Returns the info-hash.
    pub async fn add_torrent(&self, bytes: &[u8], category: &str) -> Result<String> {
        let meta = hash::parse_torrent(bytes)?;
        let dir = self.cfg.data_dir.join("torrents");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.torrent", meta.hash));
        tokio::fs::write(&path, bytes).await?;
        Ok(self.enqueue(meta, category, Source::Torrent(path)))
    }

    /// Queue a magnet, an info-hash, or an http(s) link to a `.torrent` file
    /// (which may redirect to a magnet, as indexer proxies do).
    pub async fn add_url(&self, url: &str, category: &str) -> Result<String> {
        // Redirects are followed by hand: indexer proxies may redirect to a `magnet:` URI.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut url = url.trim().to_owned();
        for _ in 0..6 {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return self.add_magnet(&url, category);
            }
            let resp = http
                .get(&url)
                .send()
                .await
                .context("fetching the .torrent link")?;
            if resp.status().is_redirection() {
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .context("redirect without a location")?;
                url = if loc.starts_with("magnet:") {
                    loc.to_owned()
                } else {
                    resp.url().join(loc)?.to_string()
                };
                continue;
            }
            let bytes = resp.error_for_status()?.bytes().await?;
            return self.add_torrent(&bytes, category).await;
        }
        anyhow::bail!("too many redirects")
    }

    /// Remove an item, its AllDebrid magnet and (optionally) its files on disk.
    pub async fn remove(&self, hash: &str, delete_files: bool) -> bool {
        let Some(item) = self.store.remove(hash) else {
            return false;
        };
        if let Source::Torrent(path) = &item.source {
            let _ = tokio::fs::remove_file(path).await;
        }
        let gone_remotely =
            item.state == store::ItemState::Completed && self.cfg.delete_after_download;
        if let (Some(id), false) = (item.magnet_id, gone_remotely) {
            if let Err(e) = self.client.delete_magnet(alldebrid::MagnetId(id)).await {
                tracing::warn!(hash, "could not remove the magnet from AllDebrid: {e}");
            }
        }
        if delete_files {
            let _ = tokio::fs::remove_dir_all(item.content_path()).await;
        }
        true
    }
}

/// Run the server and the worker until the process is stopped.
pub async fn serve(cfg: Config) -> Result<()> {
    let app = App::new(cfg)?;
    for dir in app.cfg.categories.values() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let listener = tokio::net::TcpListener::bind(app.cfg.listen).await?;
    tracing::info!("listening on http://{}", app.cfg.listen);
    tokio::spawn(worker::run(app.clone()));
    axum::serve(listener, http::router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

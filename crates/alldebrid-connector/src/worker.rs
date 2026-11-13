//! Background loop: upload queued items, follow them on AllDebrid, download finished ones.

use std::path::Path;
use std::time::Duration;

use alldebrid::{Error as AdError, FileNode, MagnetId, WaitOptions};
use anyhow::{anyhow, bail, Context, Result};
use tokio::io::AsyncWriteExt;

use crate::store::{now, sanitize_rel_path, ItemFile, ItemState, Source};
use crate::App;

/// AllDebrid refuses new magnets beyond this many active ones.
const MAX_ACTIVE: usize = 30;
const FILE_ATTEMPTS: u32 = 4;

/// Run forever.
pub async fn run(app: App) {
    loop {
        tick(&app).await;
        tokio::select! {
            () = tokio::time::sleep(app.cfg.poll_interval) => {}
            () = app.wake.notified() => {}
        }
    }
}

async fn tick(app: &App) {
    upload_queued(app).await;
    follow_processing(app).await;
}

fn fail(app: &App, hash: &str, msg: String) {
    tracing::warn!(hash, "failed: {msg}");
    app.store.update(hash, |i| {
        i.state = ItemState::Failed;
        i.error = Some(msg);
    });
}

async fn upload_queued(app: &App) {
    let items = app.store.list();
    let mut active = items
        .iter()
        .filter(|i| i.state == ItemState::Processing)
        .count();
    for item in items.into_iter().filter(|i| i.state == ItemState::Queued) {
        if active >= MAX_ACTIVE {
            break;
        }
        let res = match &item.source {
            Source::Magnet(uri) => app.client.upload_magnet(uri).await,
            Source::Torrent(path) => match tokio::fs::read(path).await {
                Ok(bytes) => {
                    app.client
                        .upload_torrent(format!("{}.torrent", item.hash), bytes)
                        .await
                }
                Err(e) => {
                    fail(
                        app,
                        &item.hash,
                        format!("stored .torrent is unreadable: {e}"),
                    );
                    continue;
                }
            },
        };
        match res {
            Ok(up) => {
                tracing::info!(hash = %item.hash, id = %up.id, name = %up.name, "sent to AllDebrid");
                app.store.update(&item.hash, |i| {
                    i.magnet_id = Some(up.id.0);
                    if !up.name.is_empty() {
                        i.name = up.name.clone();
                    }
                    if up.size > 0 {
                        i.size = up.size;
                    }
                    i.state = ItemState::Processing;
                });
                if let Source::Torrent(path) = &item.source {
                    let _ = tokio::fs::remove_file(path).await;
                }
                active += 1;
            }
            Err(e)
                if e.api_code()
                    .is_some_and(|c| c.as_str() == "MAGNET_TOO_MANY_ACTIVE") =>
            {
                tracing::info!("AllDebrid has too many active magnets; the queue waits");
                break;
            }
            Err(AdError::Api(e)) => fail(app, &item.hash, format!("AllDebrid refused it: {e}")),
            Err(e) => tracing::warn!(hash = %item.hash, "upload failed, will retry: {e}"),
        }
    }
}

async fn follow_processing(app: &App) {
    let processing: Vec<_> = app
        .store
        .list()
        .into_iter()
        .filter(|i| i.state == ItemState::Processing)
        .collect();
    if processing.is_empty() {
        return;
    }
    let magnets = match app.client.magnets(None).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("could not read magnet status: {e}");
            return;
        }
    };
    for item in processing {
        let Some(m) = magnets.iter().find(|m| Some(m.id.0) == item.magnet_id) else {
            fail(
                app,
                &item.hash,
                "the magnet is no longer in the AllDebrid account".into(),
            );
            continue;
        };
        if m.status_code.is_failed() {
            fail(
                app,
                &item.hash,
                format!("AllDebrid could not fetch it: {}", m.status),
            );
            continue;
        }
        let ready = m.status_code.is_ready();
        app.store.update(&item.hash, |i| {
            if !m.filename.is_empty() {
                i.name = m.filename.clone();
            }
            if m.size > 0 {
                i.size = m.size;
            }
            i.cloud_downloaded = m.downloaded;
            i.cloud_speed = m.download_speed;
            if ready {
                i.state = ItemState::Downloading;
                i.local_downloaded = 0;
            }
        });
        if ready {
            tokio::spawn(download_item(app.clone(), item.hash.clone()));
        }
    }
}

async fn download_item(app: App, hash: String) {
    let Ok(_permit) = app.slots.acquire().await else {
        return;
    };
    match download(&app, &hash).await {
        Ok(true) => {
            let Some(item) = app.store.get(&hash) else {
                return;
            };
            tracing::info!(hash, path = %item.content_path().display(), "download complete");
            app.store.update(&hash, |i| {
                i.state = ItemState::Completed;
                i.completed_on = Some(now());
                i.local_speed = 0;
            });
            if let (true, Some(id)) = (app.cfg.delete_after_download, item.magnet_id) {
                if let Err(e) = app.client.delete_magnet(MagnetId(id)).await {
                    tracing::warn!(hash, "could not remove the magnet from AllDebrid: {e}");
                }
            }
        }
        Ok(false) => tracing::info!(hash, "download cancelled"),
        Err(e) => fail(&app, &hash, format!("{e:#}")),
    }
}

/// Download every file of an item. `Ok(false)` means the item was removed meanwhile.
async fn download(app: &App, hash: &str) -> Result<bool> {
    let Some(item) = app.store.get(hash) else {
        return Ok(false);
    };
    let id = MagnetId(
        item.magnet_id
            .ok_or_else(|| anyhow!("item has no magnet id"))?,
    );
    let tree = app
        .client
        .magnet_file_tree(id)
        .await
        .context("listing files")?;
    let mut files = FileNode::flatten(&tree);
    if files.is_empty() {
        bail!("AllDebrid lists no files for this magnet");
    }
    // The tree often starts with a folder named like the torrent; avoid `name/name/...`.
    let prefix = format!("{}/", item.name);
    if files.iter().all(|f| f.path.starts_with(&prefix)) {
        for f in &mut files {
            f.path.drain(..prefix.len());
        }
    }
    let files: Vec<_> = files
        .into_iter()
        .map(|f| (sanitize_rel_path(&f.path), f))
        .filter(|(rel, _)| !rel.as_os_str().is_empty())
        .collect();
    if files.is_empty() {
        bail!("AllDebrid lists no usable file names for this magnet");
    }
    let total: u64 = files.iter().map(|(_, f)| f.size).sum();
    let recorded: Vec<ItemFile> = files
        .iter()
        .map(|(rel, f)| ItemFile {
            path: rel.clone(),
            size: f.size,
        })
        .collect();
    app.store.update(hash, |i| {
        i.size = total;
        i.files = recorded;
    });

    let root = item.folder();
    let mut done = 0u64;
    for (rel, f) in &files {
        let target = root.join(rel);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let on_disk = tokio::fs::metadata(&target).await.map(|m| m.len()).ok();
        if !(f.size > 0 && on_disk == Some(f.size)) {
            let opts = WaitOptions {
                poll_interval: Duration::from_secs(5),
                timeout: Some(Duration::from_secs(600)),
            };
            let mut attempt = 0;
            loop {
                attempt += 1;
                let res = async {
                    let link = app
                        .client
                        .unlock_resolved(&f.link, None, opts.clone())
                        .await?;
                    fetch(app, hash, &link.url, &target, f.size, done).await
                }
                .await;
                match res {
                    Ok(true) => break,
                    Ok(false) => return Ok(false),
                    Err(e) if attempt < FILE_ATTEMPTS => {
                        tracing::warn!(hash, file = %f.path, "attempt {attempt} failed, retrying: {e:#}");
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                    Err(e) => return Err(e.context(format!("downloading {}", f.path))),
                }
            }
        }
        done += f.size;
        if !app.store.set_progress(hash, done) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Download one URL to `target` through a resumable `.part` file.
/// `Ok(false)` means the item was removed meanwhile.
async fn fetch(
    app: &App,
    hash: &str,
    url: &str,
    target: &Path,
    expected: u64,
    base: u64,
) -> Result<bool> {
    let mut part = target.as_os_str().to_owned();
    part.push(".part");
    let part = Path::new(&part);

    let mut pos = tokio::fs::metadata(part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut req = app.http.get(url);
    if pos > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={pos}-"));
    }
    let mut resp = req.send().await?;
    if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        let _ = tokio::fs::remove_file(part).await;
        bail!("the server rejected the resume position; starting over");
    }
    let resumed = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if !resp.status().is_success() {
        bail!("HTTP {}", resp.status());
    }
    if !resumed {
        pos = 0;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(part)
        .await
        .with_context(|| format!("opening {}", part.display()))?;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        pos += chunk.len() as u64;
        if !app.store.set_progress(hash, base + pos) {
            return Ok(false);
        }
    }
    file.flush().await?;
    drop(file);
    if expected > 0 && pos != expected {
        if pos > expected {
            let _ = tokio::fs::remove_file(part).await;
        }
        bail!("size mismatch: got {pos} bytes, expected {expected}");
    }
    tokio::fs::rename(part, target).await?;
    Ok(true)
}

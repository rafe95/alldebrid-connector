//! HTTP layer: the connector's own REST API plus a qBittorrent Web API emulation.

use std::collections::HashMap;

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::store::{Item, ItemState};
use crate::App;

/// Build the router.
pub fn router(app: App) -> Router {
    let qbit = Router::new()
        .route("/auth/login", get(qb_login).post(qb_login))
        .route("/app/version", get(|| async { "v4.6.7" }))
        .route("/app/webapiVersion", get(|| async { "2.9.3" }))
        .route("/app/preferences", get(qb_preferences))
        .route("/torrents/categories", get(qb_categories))
        .route("/torrents/add", post(qb_add))
        .route("/torrents/info", get(qb_info))
        .route("/torrents/properties", get(qb_properties))
        .route("/torrents/files", get(qb_files))
        .route("/torrents/delete", get(qb_delete).post(qb_delete))
        .route("/torrents/setCategory", post(qb_set_category))
        // Everything else (pause, resume, share limits, createCategory...) is accepted and ignored.
        .fallback(|| async { "Ok." });

    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/categories", get(categories))
        .route("/api/downloads", get(list).post(add_json))
        .route("/api/downloads/torrent", post(add_file))
        .route("/api/downloads/{hash}", delete(remove).get(get_one))
        .nest("/api/v2", qbit)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(app)
}

fn bad_request(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

// --- own API -------------------------------------------------------------------

fn item_json(i: &Item) -> Value {
    json!({
        "hash": i.hash,
        "name": i.name,
        "category": i.category,
        "state": i.state,
        "error": i.error,
        "progress": i.progress(),
        "size": i.size,
        "path": i.content_path(),
        "added_on": i.added_on,
        "completed_on": i.completed_on,
    })
}

async fn categories(State(app): State<App>) -> Json<Value> {
    Json(json!(app.cfg.categories))
}

async fn list(State(app): State<App>) -> Json<Value> {
    Json(app.store.list().iter().map(item_json).collect())
}

async fn get_one(State(app): State<App>, Path(hash): Path<String>) -> Response {
    match app.store.get(&hash.to_ascii_lowercase()) {
        Some(i) => Json(item_json(&i)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct AddBody {
    /// Magnet URI, info-hash, or http(s) link to a `.torrent`.
    #[serde(alias = "url")]
    magnet: String,
    #[serde(default)]
    category: String,
}

async fn add_json(State(app): State<App>, Json(body): Json<AddBody>) -> Response {
    match app.add_url(&body.magnet, &body.category).await {
        Ok(hash) => (StatusCode::ACCEPTED, Json(json!({ "hash": hash }))).into_response(),
        Err(e) => bad_request(format!("{e:#}")),
    }
}

async fn add_file(State(app): State<App>, mut form: Multipart) -> Response {
    let mut category = String::new();
    let mut torrents = Vec::new();
    loop {
        let field = match form.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return bad_request(e),
        };
        let is_category = field.name() == Some("category");
        match field.bytes().await {
            Ok(b) if is_category => category = String::from_utf8_lossy(&b).into_owned(),
            Ok(b) => torrents.push(b),
            Err(e) => return bad_request(e),
        }
    }
    if torrents.is_empty() {
        return bad_request("no .torrent file in the request");
    }
    let mut hashes = Vec::new();
    for t in torrents {
        match app.add_torrent(&t, &category).await {
            Ok(h) => hashes.push(h),
            Err(e) => return bad_request(format!("{e:#}")),
        }
    }
    (StatusCode::ACCEPTED, Json(json!({ "hashes": hashes }))).into_response()
}

#[derive(Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    delete_files: bool,
}

async fn remove(
    State(app): State<App>,
    Path(hash): Path<String>,
    Query(q): Query<RemoveQuery>,
) -> StatusCode {
    if app.remove(&hash.to_ascii_lowercase(), q.delete_files).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

// --- qBittorrent emulation ----------------------------------------------------

async fn qb_login() -> Response {
    // No authentication: any credentials are accepted.
    (
        [(
            header::SET_COOKIE,
            "SID=alldebrid-connector; HttpOnly; path=/",
        )],
        "Ok.",
    )
        .into_response()
}

async fn qb_preferences(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "save_path": app.cfg.dir_for(""),
        "max_ratio_enabled": false,
        "max_ratio": -1,
        "max_seeding_time_enabled": false,
        "max_seeding_time": -1,
        "max_ratio_act": 0,
        "queueing_enabled": false,
        "dht": true,
    }))
}

async fn qb_categories(State(app): State<App>) -> Json<Value> {
    let mut out = serde_json::Map::new();
    for (name, dir) in &app.cfg.categories {
        out.insert(name.clone(), json!({ "name": name, "savePath": dir }));
    }
    // Labels in use that have no folder of their own are listed too, so clients can filter on them.
    for i in app.store.list() {
        out.entry(i.category.clone())
            .or_insert_with(|| json!({ "name": i.category, "savePath": i.save_path }));
    }
    Json(Value::Object(out))
}

/// Whether a completed item's files are gone from disk (or cannot be read).
fn files_missing(i: &Item) -> bool {
    let exists = |p: &std::path::Path| p.try_exists().unwrap_or(false);
    i.state == ItemState::Completed
        && if i.files.is_empty() {
            !exists(&i.content_path())
        } else {
            i.files.iter().any(|f| !exists(&i.folder().join(&f.path)))
        }
}

fn qb_state(i: &Item, missing: bool) -> &'static str {
    match i.state {
        ItemState::Queued => "queuedDL",
        ItemState::Processing | ItemState::Downloading => "downloading",
        // Radarr/Sonarr show a warning instead of failing the import.
        ItemState::Completed if missing => "missingFiles",
        // Finished and not seeding: lets Sonarr/Radarr import and then remove it.
        ItemState::Completed => "pausedUP",
        ItemState::Failed => "error",
    }
}

fn qb_torrent(i: &Item, missing: bool) -> Value {
    let progress = i.progress();
    let done = i.state == ItemState::Completed;
    let speed = match i.state {
        ItemState::Processing => i.cloud_speed,
        ItemState::Downloading => i.local_speed,
        _ => 0,
    };
    let left = (i.size as f64 * (1.0 - progress)) as u64;
    let eta = if done {
        0
    } else {
        left.checked_div(speed).unwrap_or(8_640_000)
    };
    json!({
        "hash": i.hash,
        "name": i.name,
        "size": i.size,
        "total_size": i.size,
        "progress": progress,
        "dlspeed": speed,
        "upspeed": 0,
        "eta": eta,
        "state": qb_state(i, missing),
        "category": i.category,
        "tags": "",
        "save_path": i.save_path,
        "content_path": i.content_path(),
        "amount_left": left,
        "completed": i.size - left.min(i.size),
        "downloaded": i.size - left.min(i.size),
        "added_on": i.added_on,
        "completion_on": i.completed_on.unwrap_or(-1),
        "last_activity": i.completed_on.unwrap_or(i.added_on),
        "priority": 0,
        "num_seeds": 0,
        "num_leechs": 0,
        // Report the seeding goal as met once finished, so clients are free to remove it.
        "ratio": if done { 1.0 } else { 0.0 },
        "ratio_limit": 1.0,
        "seeding_time": 0,
        "seeding_time_limit": -2,
        "inactive_seeding_time_limit": -2,
    })
}

fn select<'a>(items: &'a [Item], hashes: Option<&str>) -> Vec<&'a Item> {
    match hashes {
        None | Some("" | "all") => items.iter().collect(),
        Some(h) => {
            let wanted: Vec<String> = h
                .split('|')
                .map(|s| s.trim().to_ascii_lowercase())
                .collect();
            items.iter().filter(|i| wanted.contains(&i.hash)).collect()
        }
    }
}

async fn qb_info(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let items = app.store.list();
    let out: Vec<Value> = select(&items, q.get("hashes").map(String::as_str))
        .into_iter()
        .filter(|i| {
            q.get("category")
                .map_or(true, |c| i.category.eq_ignore_ascii_case(c))
        })
        .map(|i| qb_torrent(i, files_missing(i)))
        .collect();
    Json(json!(out))
}

async fn qb_properties(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let hash = q
        .get("hash")
        .map(|h| h.to_ascii_lowercase())
        .unwrap_or_default();
    match app.store.get(&hash) {
        Some(i) => Json(json!({
            "save_path": i.save_path,
            "total_size": i.size,
            "addition_date": i.added_on,
            "completion_date": i.completed_on.unwrap_or(-1),
            "seeding_time": 0,
            "share_ratio": if i.state == ItemState::Completed { 1.0 } else { 0.0 },
        }))
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn qb_files(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let hash = q
        .get("hash")
        .map(|h| h.to_ascii_lowercase())
        .unwrap_or_default();
    match app.store.get(&hash) {
        Some(i) => Json(qb_file_list(&i)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `torrents/files`: one entry per file, `name` relative to `save_path` with `/`
/// separators, so `save_path/name` is where the file is (and, for a single file,
/// equals `content_path`).
fn qb_file_list(i: &Item) -> Value {
    let folder = i.folder_name();
    if i.files.is_empty() {
        return json!([{
            "index": 0, "name": folder, "size": i.size, "progress": i.progress(),
            "priority": 1, "is_seed": false, "piece_range": [0, 0], "availability": 0,
        }]);
    }
    let mut offset = 0u64;
    let entries: Vec<Value> = i
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| {
            let progress = match i.state {
                ItemState::Completed => 1.0,
                ItemState::Downloading if f.size == 0 => 0.0,
                ItemState::Downloading => {
                    (i.local_downloaded.saturating_sub(offset) as f64 / f.size as f64).min(1.0)
                }
                _ => 0.0,
            };
            offset += f.size;
            let rel: Vec<String> = f
                .path
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            json!({
                "index": index,
                "name": format!("{folder}/{}", rel.join("/")),
                "size": f.size,
                "progress": progress,
                "priority": 1,
                "is_seed": progress >= 1.0,
                "piece_range": [0, 0],
                "availability": if progress >= 1.0 { 1.0 } else { 0.0 },
            })
        })
        .collect();
    json!(entries)
}

/// Fields of a request sent either as a query string, a urlencoded form or a multipart form.
struct Fields {
    text: HashMap<String, String>,
    files: Vec<Vec<u8>>,
}

#[allow(clippy::result_large_err)]
async fn fields(req: Request) -> Result<Fields, Response> {
    let mut out = Fields {
        text: HashMap::new(),
        files: Vec::new(),
    };
    if let Some(q) = req.uri().query() {
        out.text
            .extend(url::form_urlencoded::parse(q.as_bytes()).into_owned());
    }
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.starts_with("multipart/form-data") {
        let mut form = Multipart::from_request(req, &())
            .await
            .map_err(bad_request)?;
        while let Some(field) = form.next_field().await.map_err(bad_request)? {
            let name = field.name().unwrap_or_default().to_owned();
            let is_file = field.file_name().is_some() || name == "torrents";
            let bytes = field.bytes().await.map_err(bad_request)?;
            if is_file {
                out.files.push(bytes.to_vec());
            } else {
                out.text
                    .insert(name, String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    } else if content_type.starts_with("application/x-www-form-urlencoded") {
        let Form(map) = Form::<HashMap<String, String>>::from_request(req, &())
            .await
            .map_err(bad_request)?;
        out.text.extend(map);
    }
    Ok(out)
}

async fn qb_add(State(app): State<App>, req: Request) -> Response {
    let f = match fields(req).await {
        Ok(f) => f,
        Err(r) => return r,
    };
    let category = f.text.get("category").cloned().unwrap_or_default();
    let urls: Vec<&str> = f
        .text
        .get("urls")
        .map(|u| u.lines().map(str::trim).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default();
    if urls.is_empty() && f.files.is_empty() {
        return (StatusCode::BAD_REQUEST, "Fails.").into_response();
    }
    for url in urls {
        if let Err(e) = app.add_url(url, &category).await {
            tracing::warn!("rejected {url:?}: {e:#}");
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Fails.").into_response();
        }
    }
    for file in &f.files {
        if let Err(e) = app.add_torrent(file, &category).await {
            tracing::warn!("rejected a .torrent file: {e:#}");
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Fails.").into_response();
        }
    }
    "Ok.".into_response()
}

async fn qb_delete(State(app): State<App>, req: Request) -> Response {
    let f = match fields(req).await {
        Ok(f) => f,
        Err(r) => return r,
    };
    // Unlike `info`, a missing `hashes` must not mean "everything" here.
    let Some(hashes) = f.text.get("hashes").filter(|h| !h.is_empty()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let delete_files = f
        .text
        .get("deleteFiles")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let items = app.store.list();
    for item in select(&items, Some(hashes)) {
        app.remove(&item.hash, delete_files).await;
    }
    "Ok.".into_response()
}

async fn qb_set_category(State(app): State<App>, req: Request) -> Response {
    let f = match fields(req).await {
        Ok(f) => f,
        Err(r) => return r,
    };
    let category = f.text.get("category").cloned().unwrap_or_default();
    let Some(hashes) = f.text.get("hashes").filter(|h| !h.is_empty()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let items = app.store.list();
    for item in select(&items, Some(hashes)) {
        // Only the label changes; files already on disk are not moved.
        app.store
            .update(&item.hash, |i| i.category = category.clone());
    }
    "Ok.".into_response()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::store::{ItemFile, Source};

    const GRINCH: &str = "How.the.Grinch.Stole.Christmas.2000.2160p.UHD.Blu-ray.Remux.DV.HDR.HEVC.TrueHD.Atmos.7.1-CiNEPHiLES.mkv";

    /// A completed item whose files exist on disk below `save`.
    fn on_disk(save: &Path, name: &str, files: &[(&str, &str)]) -> Item {
        let mut i = Item::new(
            "h".into(),
            name.into(),
            "movies".into(),
            save.to_owned(),
            Source::Magnet("m".into()),
        );
        for (rel, body) in files {
            let path = i.folder().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            i.files.push(ItemFile {
                path: rel.into(),
                size: body.len() as u64,
            });
        }
        i.size = i.files.iter().map(|f| f.size).sum();
        i.state = ItemState::Completed;
        i.completed_on = Some(1);
        i
    }

    /// `torrents/info` and `torrents/files` for an item, through a JSON round trip.
    fn api(i: &Item) -> (Value, Value) {
        let text = |v: Value| serde_json::to_string(&v).unwrap();
        let info = text(qb_torrent(i, files_missing(i)));
        let files = text(qb_file_list(i));
        (
            serde_json::from_str(&info).unwrap(),
            serde_json::from_str(&files).unwrap(),
        )
    }

    fn path_of(v: &Value) -> PathBuf {
        PathBuf::from(v.as_str().unwrap())
    }

    /// `save_path` + each `files[].name` must exist, and match `content_path` for one file.
    fn assert_consistent(info: &Value, files: &Value) {
        let save = path_of(&info["save_path"]);
        let names: Vec<PathBuf> = files
            .as_array()
            .unwrap()
            .iter()
            .map(|f| save.join(f["name"].as_str().unwrap()))
            .collect();
        for n in &names {
            assert!(n.is_file(), "{} is not a file", n.display());
        }
        let content = path_of(&info["content_path"]);
        if names.len() == 1 {
            assert_eq!(content, names[0]);
        } else {
            assert!(content.is_dir());
            assert!(names.iter().all(|n| n.starts_with(&content)));
        }
    }

    fn assert_completed(info: &Value, size: u64) {
        assert_eq!(info["state"], "pausedUP");
        assert_eq!(info["progress"], 1.0);
        assert_eq!(info["amount_left"], 0);
        assert_eq!(info["completed"], size);
        assert_eq!(info["total_size"], size);
        assert_eq!(info["category"], "movies");
    }

    #[test]
    fn single_mkv_inside_a_folder_named_after_the_release() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("movies");
        let i = on_disk(&save, GRINCH, &[(GRINCH, "0123456789")]);
        let (info, files) = api(&i);
        assert_eq!(
            path_of(&info["content_path"]),
            save.join(GRINCH).join(GRINCH)
        );
        assert!(path_of(&info["content_path"]).is_file());
        assert_eq!(path_of(&info["save_path"]), save);
        assert_eq!(files[0]["name"], format!("{GRINCH}/{GRINCH}"));
        assert_completed(&info, 10);
        assert_consistent(&info, &files);
    }

    #[test]
    fn single_mp4() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("movies");
        let i = on_disk(&save, "Some.Movie.2020.1080p", &[("some.movie.mp4", "abc")]);
        let (info, files) = api(&i);
        assert_eq!(
            path_of(&info["content_path"]),
            save.join("Some.Movie.2020.1080p/some.movie.mp4")
        );
        assert_completed(&info, 3);
        assert_consistent(&info, &files);
    }

    #[test]
    fn multi_file_reports_the_root_folder() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("movies");
        let i = on_disk(
            &save,
            "Trilogy.Pack",
            &[
                ("Part.1.mkv", "1"),
                ("Part.2.mkv", "22"),
                ("Part.3.mkv", "333"),
            ],
        );
        let (info, files) = api(&i);
        assert_eq!(path_of(&info["content_path"]), save.join("Trilogy.Pack"));
        assert_eq!(files.as_array().unwrap().len(), 3);
        assert_eq!(files[2]["index"], 2);
        assert_eq!(files[2]["name"], "Trilogy.Pack/Part.3.mkv");
        assert_completed(&info, 6);
        assert_consistent(&info, &files);
    }

    #[test]
    fn multi_file_with_subtitles_keeps_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("movies");
        let i = on_disk(
            &save,
            "Movie.2019",
            &[
                ("Movie.2019.mkv", "video"),
                ("Subs/English.srt", "en"),
                ("Subs/French.srt", "fr"),
            ],
        );
        let (info, files) = api(&i);
        assert_eq!(path_of(&info["content_path"]), save.join("Movie.2019"));
        assert_eq!(files[1]["name"], "Movie.2019/Subs/English.srt");
        assert_completed(&info, 9);
        assert_consistent(&info, &files);
    }

    #[test]
    fn incomplete_download() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("movies");
        let mut i = on_disk(&save, "Pack", &[("a.mkv", "aaaa"), ("b.mkv", "bbbb")]);
        i.state = ItemState::Downloading;
        i.completed_on = None;
        i.local_downloaded = 6;
        let (info, files) = api(&i);
        assert_eq!(info["state"], "downloading");
        assert!(info["progress"].as_f64().unwrap() < 1.0);
        assert!(info["amount_left"].as_u64().unwrap() > 0);
        assert_eq!(info["completion_on"], -1);
        assert_eq!(files[0]["progress"], 1.0);
        assert_eq!(files[1]["progress"], 0.5);

        // Before AllDebrid has listed the files, the item folder is reported.
        i.files.clear();
        i.state = ItemState::Processing;
        let (info, files) = api(&i);
        assert_eq!(path_of(&info["content_path"]), save.join("Pack"));
        assert_eq!(files[0]["name"], "Pack");
    }

    #[test]
    fn special_characters_survive_json() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("movies");
        let name = r#"Amélie [2001] {Remux} (Director's Cut) 東京 "quoted" \ back & 100%"#;
        let file = r#"Amélie [2001] "quoted" \ 東京 #1.mkv"#;
        let i = on_disk(&save, name, &[(file, "x")]);
        let raw = serde_json::to_string(&qb_torrent(&i, false)).unwrap();
        // Every escape is a valid JSON one (never `\.`, `\[`, `\ `...).
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                let next = chars.next().unwrap();
                assert!(
                    r#""\/bfnrtu"#.contains(next),
                    "invalid escape \\{next} in {raw}"
                );
            }
        }
        let (info, files) = api(&i);
        let folder = i.folder_name();
        assert_eq!(info["name"], name);
        assert_eq!(
            path_of(&info["content_path"]),
            save.join(&folder).join(file)
        );
        assert_consistent(&info, &files);
    }

    #[test]
    fn missing_or_unreadable_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("movies");
        let i = on_disk(&save, GRINCH, &[(GRINCH, "x")]);
        assert_eq!(api(&i).0["state"], "pausedUP");
        std::fs::remove_file(i.content_path()).unwrap();
        assert_eq!(api(&i).0["state"], "missingFiles");

        let i = on_disk(&save, "Pack", &[("a.mkv", "a"), ("b.srt", "b")]);
        std::fs::remove_file(i.folder().join("b.srt")).unwrap();
        assert_eq!(api(&i).0["state"], "missingFiles");

        // Not completed: nothing is expected on disk yet.
        let mut i = on_disk(&save, "Later", &[]);
        i.state = ItemState::Downloading;
        assert_eq!(api(&i).0["state"], "downloading");
    }
}

//! End-to-end: the real server and worker against a mock AllDebrid and file host.

use std::path::Path;
use std::time::Duration;

use alldebrid_connector::{http, worker, App, Config};
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "abcdef0123456789abcdef0123456789abcdef01";

fn ok(data: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "status": "success", "data": data }))
}

/// A mock AllDebrid holding one ready magnet `My.Show` with a single 11-byte file.
async fn mock_alldebrid() -> MockServer {
    mock_alldebrid_with(
        "My.Show",
        json!([{ "n": "My.Show", "e": [ { "n": "ep1.mkv", "s": 11, "l": "https://alldebrid.com/f/1" } ] }]),
    )
    .await
}

/// A mock AllDebrid holding one ready magnet `name` with the given file tree; every
/// file unlocks to `/dl/ep1.mkv`.
async fn mock_alldebrid_with(name: &str, tree: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/magnet/upload"))
        .respond_with(ok(json!({ "magnets": [
            { "magnet": "m", "hash": HASH, "name": name, "size": 11, "ready": false, "id": 7 }
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4.1/magnet/status"))
        .respond_with(ok(json!({ "magnets": [
            { "id": 7, "filename": name, "size": 11, "status": "Ready", "statusCode": 4, "downloaded": 11 }
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4/magnet/files"))
        .respond_with(ok(json!({ "magnets": [{ "id": "7", "files": tree }]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4/link/unlock"))
        .respond_with(ok(json!({
            "link": format!("{}/dl/ep1.mkv", server.uri()), "filename": "ep1.mkv",
            "host": "magnet", "filesize": 11, "id": "g1"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4/magnet/delete"))
        .respond_with(ok(json!({ "message": "deleted" })))
        .expect(1)
        .mount(&server)
        .await;
    server
}

/// Start the connector on a random port; returns its base URL.
async fn start(server: &MockServer, root: &Path) -> String {
    let mut cfg = Config::new("test-key", &root.join("data"), &root.join("downloads"));
    cfg.base_url = Some(server.uri());
    cfg.poll_interval = Duration::from_millis(20);
    let app = App::new(cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(worker::run(app.clone()));
    tokio::spawn(async move { axum::serve(listener, http::router(app)).await.unwrap() });
    url
}

async fn wait_for_state(http: &reqwest::Client, url: &str, state: &str) -> Value {
    wait_for_state_in(http, url, "tv", state).await
}

async fn wait_for_state_in(
    http: &reqwest::Client,
    url: &str,
    category: &str,
    state: &str,
) -> Value {
    for _ in 0..250 {
        let list: Value = http
            .get(format!("{url}/api/v2/torrents/info?category={category}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if list[0]["state"] == state {
            return list[0].clone();
        }
        assert_ne!(list[0]["state"], "error", "item failed: {list}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never reached state {state}");
}

#[tokio::test]
async fn qbittorrent_client_flow_downloads_into_category_folder() {
    let server = mock_alldebrid().await;
    Mock::given(method("GET"))
        .and(path("/dl/ep1.mkv"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello world"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let url = start(&server, dir.path()).await;
    let http = reqwest::Client::new();

    // What Prowlarr/Sonarr do when testing and using a qBittorrent client.
    let login = http
        .post(format!("{url}/api/v2/auth/login"))
        .send()
        .await
        .unwrap();
    assert!(login.headers().contains_key("set-cookie"));
    assert_eq!(login.text().await.unwrap(), "Ok.");
    let cats: Value = http
        .get(format!("{url}/api/v2/torrents/categories"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cats["tv"]["savePath"]
        .as_str()
        .unwrap()
        .ends_with("downloads/tv"));

    let form = reqwest::multipart::Form::new()
        .text("urls", format!("magnet:?xt=urn:btih:{HASH}&dn=My.Show"))
        .text("category", "tv");
    let res = http
        .post(format!("{url}/api/v2/torrents/add"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), "Ok.");

    let t = wait_for_state(&http, &url, "pausedUP").await;
    assert_eq!(t["hash"], HASH);
    assert_eq!(t["progress"], 1.0);
    let file = dir.path().join("downloads/tv/My.Show/ep1.mkv");
    // A single file: `content_path` is the file itself.
    assert_eq!(t["content_path"], file.to_str().unwrap());
    assert_eq!(
        t["save_path"],
        dir.path().join("downloads/tv").to_str().unwrap()
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");

    // Other categories do not see it; the item survives in the state file.
    let none: Value = http
        .get(format!("{url}/api/v2/torrents/info?category=movies"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(none, json!([]));
    assert!(std::fs::read_to_string(dir.path().join("data/state.json"))
        .unwrap()
        .contains(HASH));

    // Removing with deleteFiles cleans the folder up.
    let res = http
        .post(format!("{url}/api/v2/torrents/delete"))
        .form(&[("hashes", HASH), ("deleteFiles", "true")])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    assert!(!file.exists());
    let list: Value = http
        .get(format!("{url}/api/downloads"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list, json!([]));
    // `expect(1)` on upload and delete is verified when the mock server drops.
}

#[tokio::test]
async fn rest_api_resumes_a_partial_file_and_unknown_category_goes_to_other() {
    let server = mock_alldebrid().await;
    Mock::given(method("GET"))
        .and(path("/dl/ep1.mkv"))
        .and(header("range", "bytes=6-"))
        .respond_with(ResponseTemplate::new(206).set_body_string("world"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("downloads/other/My.Show");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("ep1.mkv.part"), "hello ").unwrap();

    let url = start(&server, dir.path()).await;
    let http = reqwest::Client::new();
    let res = http
        .post(format!("{url}/api/downloads"))
        .json(&json!({ "magnet": format!("magnet:?xt=urn:btih:{HASH}"), "category": "linux-isos" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);

    for _ in 0..250 {
        let item: Value = http
            .get(format!("{url}/api/downloads/{HASH}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(item["state"], "failed", "{item}");
        if item["state"] == "completed" {
            assert_eq!(item["category"], "linux-isos");
            assert_eq!(
                std::fs::read_to_string(folder.join("ep1.mkv")).unwrap(),
                "hello world"
            );
            assert!(!folder.join("ep1.mkv.part").exists());
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never completed");
}

#[tokio::test]
async fn rejects_garbage() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let url = start(&server, dir.path()).await;
    let http = reqwest::Client::new();
    let res = http
        .post(format!("{url}/api/downloads"))
        .json(&json!({ "magnet": "nonsense" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let form = reqwest::multipart::Form::new().part(
        "torrent",
        reqwest::multipart::Part::bytes(b"junk".to_vec()).file_name("x.torrent"),
    );
    let res = http
        .post(format!("{url}/api/downloads/torrent"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let res = http
        .post(format!("{url}/api/v2/torrents/delete"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
}

/// Add a magnet in `category` through the qBittorrent API and wait until it is done.
async fn add_and_complete(url: &str, category: &str) -> (reqwest::Client, Value, Value) {
    let http = reqwest::Client::new();
    let form = reqwest::multipart::Form::new()
        .text("urls", format!("magnet:?xt=urn:btih:{HASH}"))
        .text("category", category.to_owned());
    let res = http
        .post(format!("{url}/api/v2/torrents/add"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), "Ok.");
    let t = wait_for_state_in(&http, url, category, "pausedUP").await;
    let files: Value = http
        .get(format!("{url}/api/v2/torrents/files?hash={HASH}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (http, t, files)
}

/// Wait until the worker has removed the magnet from AllDebrid (it does so right after
/// completing), so the mock's `expect(1)` is not raced by the end of the test.
async fn wait_for_magnet_delete(server: &MockServer) {
    for _ in 0..250 {
        let reqs = server.received_requests().await.unwrap_or_default();
        if reqs.iter().any(|r| r.url.path() == "/v4/magnet/delete") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the magnet was never deleted");
}

async fn serve_file(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/dl/ep1.mkv"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello world"))
        .mount(server)
        .await;
}

/// The layout Radarr failed on in 0.1.0: a single file named like the release, stored in
/// a folder of the same name. `content_path` must be the file, not the folder.
#[tokio::test]
async fn single_file_named_like_the_release_reports_the_file() {
    let name = "How.the.Grinch.Stole.Christmas.2000.2160p.UHD.Blu-ray.Remux.DV.HDR.HEVC.TrueHD.Atmos.7.1-CiNEPHiLES.mkv";
    let server = mock_alldebrid_with(
        name,
        json!([{ "n": name, "s": 11, "l": "https://alldebrid.com/f/1" }]),
    )
    .await;
    serve_file(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let url = start(&server, dir.path()).await;
    let (_, t, files) = add_and_complete(&url, "movies").await;
    wait_for_magnet_delete(&server).await;

    let save = dir.path().join("downloads/movies");
    let file = save.join(name).join(name);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    assert_eq!(t["content_path"], file.to_str().unwrap());
    assert_eq!(t["save_path"], save.to_str().unwrap());
    assert_eq!(t["category"], "movies");
    assert_eq!(t["progress"], 1.0);
    assert_eq!(t["amount_left"], 0);
    assert_eq!(t["completed"], 11);
    assert_eq!(t["total_size"], 11);
    assert_eq!(files.as_array().unwrap().len(), 1);
    assert_eq!(files[0]["name"], format!("{name}/{name}"));
    assert_eq!(files[0]["size"], 11);
}

#[tokio::test]
async fn multi_file_keeps_its_folder_and_reports_it() {
    let server = mock_alldebrid_with(
        "Movie.2019",
        json!([{ "n": "Movie.2019", "e": [
            { "n": "Movie.2019.mkv", "s": 11, "l": "https://alldebrid.com/f/1" },
            { "n": "Subs", "e": [ { "n": "English.srt", "s": 11, "l": "https://alldebrid.com/f/2" } ] }
        ] }]),
    )
    .await;
    serve_file(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let url = start(&server, dir.path()).await;
    let (http, t, files) = add_and_complete(&url, "movies").await;
    wait_for_magnet_delete(&server).await;

    let root = dir.path().join("downloads/movies/Movie.2019");
    assert!(root.join("Movie.2019.mkv").is_file());
    assert!(root.join("Subs/English.srt").is_file());
    assert_eq!(t["content_path"], root.to_str().unwrap());
    assert_eq!(t["total_size"], 22);
    let names: Vec<&str> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["Movie.2019/Movie.2019.mkv", "Movie.2019/Subs/English.srt"]
    );

    // A file disappearing from disk shows up as `missingFiles`.
    std::fs::remove_file(root.join("Subs/English.srt")).unwrap();
    let t = wait_for_state_in(&http, &url, "movies", "missingFiles").await;
    assert_eq!(t["content_path"], root.to_str().unwrap());
}

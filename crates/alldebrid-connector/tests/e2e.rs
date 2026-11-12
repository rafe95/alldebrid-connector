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
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/magnet/upload"))
        .respond_with(ok(json!({ "magnets": [
            { "magnet": "m", "hash": HASH, "name": "My.Show", "size": 11, "ready": false, "id": 7 }
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4.1/magnet/status"))
        .respond_with(ok(json!({ "magnets": [
            { "id": 7, "filename": "My.Show", "size": 11, "status": "Ready", "statusCode": 4, "downloaded": 11 }
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4/magnet/files"))
        .respond_with(ok(json!({ "magnets": [{ "id": "7", "files": [
            { "n": "My.Show", "e": [ { "n": "ep1.mkv", "s": 11, "l": "https://alldebrid.com/f/1" } ] }
        ]}]})))
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
    for _ in 0..250 {
        let list: Value = http
            .get(format!("{url}/api/v2/torrents/info?category=tv"))
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
    assert_eq!(t["content_path"], file.parent().unwrap().to_str().unwrap());
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

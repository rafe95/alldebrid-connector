//! Integration tests against a mock AllDebrid server.

use std::time::Duration;

use alldebrid::{
    ApiErrorCode, Client, Error, FileNode, MagnetEvent, MagnetId, RetryPolicy, StatusCode,
    WaitOptions, WatchOptions,
};
use futures_util::StreamExt;
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    Client::builder()
        .api_key("test-key")
        .base_url(server.uri())
        .no_rate_limit()
        .retry(RetryPolicy {
            max_retries: 2,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            jitter: false,
        })
        .build()
        .unwrap()
}

fn ok(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "status": "success", "data": data }))
}

fn api_err(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({ "status": "error", "error": { "code": code, "message": "msg" } }))
}

fn fast() -> WaitOptions {
    WaitOptions {
        poll_interval: Duration::from_millis(5),
        timeout: Some(Duration::from_secs(5)),
    }
}

#[tokio::test]
async fn upload_magnets_sends_auth_and_parses_mixed_results() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/magnet/upload"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_string_contains("magnets%5B%5D=abc"))
        .respond_with(ok(json!({ "magnets": [
            { "magnet": "abc", "hash": "abc", "name": "Ubuntu", "size": 100, "ready": true, "id": 11 },
            { "magnet": "bad", "error": { "code": "MAGNET_INVALID_URI", "message": "invalid" } }
        ]})))
        .expect(1)
        .mount(&server)
        .await;

    let res = client(&server)
        .upload_magnets(&["abc", "bad"])
        .await
        .unwrap();
    assert_eq!(res.len(), 2);
    let first = res[0].result.as_ref().unwrap();
    assert_eq!(first.id, MagnetId(11));
    assert!(first.ready);
    assert_eq!(res[1].input, "bad");
    assert_eq!(
        res[1].result.as_ref().unwrap_err().code,
        ApiErrorCode::MagnetInvalidUri
    );
}

#[tokio::test]
async fn retries_on_429_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(ok(json!({ "magnets": [] })))
        .expect(1)
        .mount(&server)
        .await;

    let magnets = client(&server).magnets(None).await.unwrap();
    assert!(magnets.is_empty());
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let server = MockServer::start().await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .expect(3) // first try + 2 retries
        .mount(&server)
        .await;

    let err = client(&server).magnets(None).await.unwrap_err();
    assert!(matches!(err, Error::RateLimited { retry_after: Some(d) } if d.is_zero()));
}

#[tokio::test]
async fn api_errors_are_not_retried() {
    let server = MockServer::start().await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(api_err("AUTH_BAD_APIKEY"))
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server).magnets(None).await.unwrap_err();
    assert!(err.is_auth());
}

#[tokio::test]
async fn magnet_uploads_are_retried_on_503() {
    // Re-uploading a hash returns the same magnet id, so retries cannot duplicate it.
    let server = MockServer::start().await;
    Mock::given(path("/v4/magnet/upload"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;

    let err = client(&server).upload_magnets(&["abc"]).await.unwrap_err();
    assert!(matches!(err, Error::RateLimited { .. }));
}

#[tokio::test]
async fn torrent_uploads_are_not_retried_on_503() {
    let server = MockServer::start().await;
    Mock::given(path("/v4/magnet/upload/file"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server)
        .upload_torrent("a.torrent", b"d4:infode".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::RateLimited { .. }));
}

#[tokio::test]
async fn non_json_error_body_is_reported() {
    let server = MockServer::start().await;
    Mock::given(path("/v4/magnet/delete"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let err = client(&server)
        .delete_magnet(MagnetId(1))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Http { status: 404, ref body } if body == "not found"));
}

#[tokio::test]
async fn magnet_status_accepts_single_object() {
    let server = MockServer::start().await;
    Mock::given(path("/v4.1/magnet/status"))
        .and(body_string_contains("id=5"))
        .respond_with(ok(json!({ "magnets": {
            "id": 5, "filename": "x", "size": 10, "status": "Downloading", "statusCode": 1,
            "downloaded": 3, "uploaded": 0, "seeders": 4, "downloadSpeed": 1, "uploadSpeed": 0,
            "uploadDate": 1, "completionDate": 0
        }})))
        .mount(&server)
        .await;

    let m = client(&server).magnet_status(MagnetId(5)).await.unwrap();
    assert_eq!(m.status_code, StatusCode::Downloading);
    assert_eq!(m.seeders, 4);
}

#[tokio::test]
async fn wait_until_ready_polls_until_ready() {
    let server = MockServer::start().await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(ok(
            json!({ "magnets": [{ "id": 5, "status": "Downloading", "statusCode": 1 }] }),
        ))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(ok(
            json!({ "magnets": [{ "id": 5, "status": "Ready", "statusCode": 4 }] }),
        ))
        .mount(&server)
        .await;

    let m = client(&server)
        .wait_until_ready(MagnetId(5), fast())
        .await
        .unwrap();
    assert!(m.status_code.is_ready());
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn wait_until_ready_reports_failure() {
    let server = MockServer::start().await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(ok(
            json!({ "magnets": [{ "id": 5, "status": "No peer", "statusCode": 15 }] }),
        ))
        .mount(&server)
        .await;

    let err = client(&server)
        .wait_until_ready(MagnetId(5), fast())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::MagnetFailed {
            code: StatusCode::Failed(15),
            ..
        }
    ));
}

#[tokio::test]
async fn wait_until_ready_times_out() {
    let server = MockServer::start().await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(ok(json!({ "magnets": [{ "id": 5, "statusCode": 1 }] })))
        .mount(&server)
        .await;

    let opts = WaitOptions {
        poll_interval: Duration::from_millis(10),
        timeout: Some(Duration::from_millis(50)),
    };
    let err = client(&server)
        .wait_until_ready(MagnetId(5), opts)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout));
}

#[tokio::test]
async fn file_tree_and_unlock() {
    let server = MockServer::start().await;
    Mock::given(path("/v4/magnet/files"))
        .and(body_string_contains("id%5B%5D=5"))
        .respond_with(ok(json!({ "magnets": [{ "id": "5", "files": [
            { "n": "Show", "e": [ { "n": "e1.mkv", "s": 100, "l": "https://alldebrid.com/f/1" } ] }
        ]}]})))
        .mount(&server)
        .await;
    Mock::given(path("/v4/link/unlock"))
        .respond_with(ok(json!({
            "link": "https://cdn.example/e1.mkv", "filename": "e1.mkv", "host": "magnet",
            "filesize": 100, "id": "gen1", "streams": []
        })))
        .mount(&server)
        .await;

    let c = client(&server);
    let files = FileNode::flatten(&c.magnet_file_tree(MagnetId(5)).await.unwrap());
    assert_eq!(files[0].path, "Show/e1.mkv");
    let direct = c
        .unlock_resolved(&files[0].link, None, fast())
        .await
        .unwrap();
    assert_eq!(direct.url, "https://cdn.example/e1.mkv");
    assert_eq!(direct.size, 100);
}

#[tokio::test]
async fn unlock_resolved_waits_for_delayed_link() {
    let server = MockServer::start().await;
    Mock::given(path("/v4/link/unlock"))
        .respond_with(ok(
            json!({ "link": "", "filename": "big.rar", "filesize": 9, "id": "g", "delayed": 77 }),
        ))
        .mount(&server)
        .await;
    Mock::given(path("/v4/link/delayed"))
        .and(body_string_contains("id=77"))
        .respond_with(ok(json!({ "status": 1, "time_left": 5 })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/v4/link/delayed"))
        .respond_with(ok(
            json!({ "status": 2, "time_left": 0, "link": "https://cdn.example/big.rar" }),
        ))
        .mount(&server)
        .await;

    let d = client(&server)
        .unlock_resolved("https://alldebrid.com/f/x", None, fast())
        .await
        .unwrap();
    assert_eq!(d.url, "https://cdn.example/big.rar");
    assert_eq!(d.filename, "big.rar");
}

#[tokio::test]
async fn watch_emits_events_from_live_mode() {
    let server = MockServer::start().await;
    Mock::given(path("/v4.1/magnet/status"))
        .and(body_string_contains("counter=0"))
        .respond_with(ok(json!({ "fullsync": true, "counter": 1, "magnets": [
            { "id": 1, "filename": "a", "status": "Downloading", "statusCode": 1, "downloaded": 0 }
        ]})))
        .mount(&server)
        .await;
    Mock::given(path("/v4.1/magnet/status"))
        .and(body_string_contains("counter=1"))
        .respond_with(ok(json!({ "counter": 2, "magnets": [
            { "id": 1, "status": "Ready", "statusCode": 4, "downloaded": 10 }
        ]})))
        .mount(&server)
        .await;
    Mock::given(path("/v4.1/magnet/status"))
        .respond_with(ok(json!({ "counter": 2, "magnets": [] })))
        .mount(&server)
        .await;

    let events: Vec<MagnetEvent> = client(&server)
        .watch_magnets(WatchOptions {
            interval: Duration::from_millis(5),
        })
        .take(3)
        .map(Result::unwrap)
        .collect()
        .await;

    assert!(matches!(&events[0], MagnetEvent::Added(m) if m.filename == "a"));
    assert!(
        matches!(&events[1], MagnetEvent::Updated(m) if m.downloaded == 10 && m.filename == "a")
    );
    assert!(matches!(&events[2], MagnetEvent::Ready(m) if m.id == MagnetId(1)));
}

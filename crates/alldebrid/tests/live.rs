//! Tests against the real AllDebrid API. Ignored by default.
//!
//! ```sh
//! ALLDEBRID_API_KEY=... cargo test -p alldebrid --test live -- --ignored --nocapture
//! # optional: also upload a magnet, wait for it and unlock its files
//! ALLDEBRID_TEST_MAGNET='magnet:?xt=urn:btih:...' ALLDEBRID_API_KEY=... cargo test ...
//! ```
//!
//! Without `ALLDEBRID_API_KEY`, the demo key `staticDemoApikeyPrem` is used.

use std::time::Duration;

use alldebrid::{Client, FileNode, LiveSession, WaitOptions};

fn client() -> Client {
    let key = std::env::var("ALLDEBRID_API_KEY").unwrap_or_else(|_| "staticDemoApikeyPrem".into());
    Client::new(key).unwrap()
}

#[tokio::test]
#[ignore = "hits the real API"]
async fn list_magnets() {
    let magnets = client().magnets(None).await.unwrap();
    println!("{} magnets", magnets.len());
    for m in magnets.iter().take(5) {
        println!("{m:#?}");
    }
}

#[tokio::test]
#[ignore = "hits the real API"]
async fn live_mode_round_trip() {
    let c = client();
    let mut s = LiveSession::new();
    let first = c.live_status(&mut s).await.unwrap();
    println!(
        "first: fullsync={} n={} counter={}",
        first.fullsync,
        first.magnets.len(),
        s.counter
    );
    assert!(first.fullsync);
    let second = c.live_status(&mut s).await.unwrap();
    println!(
        "second: fullsync={} n={} counter={}",
        second.fullsync,
        second.magnets.len(),
        s.counter
    );
}

#[tokio::test]
#[ignore = "hits the real API"]
async fn upload_wait_unlock() {
    let Ok(magnet) = std::env::var("ALLDEBRID_TEST_MAGNET") else {
        eprintln!("ALLDEBRID_TEST_MAGNET not set, skipping");
        return;
    };
    let c = client();
    let up = c.upload_magnet(&magnet).await.unwrap();
    println!("uploaded: {up:#?}");
    let ready = c
        .wait_until_ready(
            up.id,
            WaitOptions {
                timeout: Some(Duration::from_secs(600)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    println!("ready: {ready:#?}");
    for f in FileNode::flatten(&c.magnet_file_tree(up.id).await.unwrap()) {
        let d = c
            .unlock_resolved(&f.link, None, WaitOptions::default())
            .await
            .unwrap();
        println!("{} ({} bytes) -> {}", f.path, f.size, d.url);
    }
}

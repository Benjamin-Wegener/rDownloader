//! Live smoke test: reaches a real onion service through Tor.
//!
//! Needs internet access and takes a minute (Tor bootstrap), so it is ignored by
//! default. Run it with `cargo test --test live_onion -- --ignored --nocapture`.

use std::sync::Arc;

use rdownloader::contract::Transport;
use rdownloader::net::transport::TorTransport;
use rdownloader::tor::TorManager;

const ONION_URL: &str = "http://duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad.onion/";

#[tokio::test]
#[ignore = "needs internet access and a Tor bootstrap"]
async fn fetches_head_from_an_onion_service() {
    let dir = std::env::temp_dir().join(format!("rdl-live-onion-{}", uuid::Uuid::new_v4()));
    let tor = Arc::new(
        TorManager::bootstrap(dir.clone(), &[])
            .await
            .expect("bootstrap"),
    );
    let transport = TorTransport::new(tor);

    let url = url::Url::parse(ONION_URL).unwrap();
    let head = transport.head(&url, 1).await.expect("HEAD over onion");
    assert!(head.status < 500, "unexpected status {}", head.status);

    let _ = std::fs::remove_dir_all(dir);
}

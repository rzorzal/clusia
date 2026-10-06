mod common;

use std::sync::Arc;

use clusia_platform::{MemoryStore, SecretStore};
use clusia_protocol::{Client, Command, Reply};
use common::{TestDaemon, test_options};

async fn configured(c: &mut Client) -> bool {
    match c.request(Command::GiphyKeyStatus).await {
        Ok(Reply::GiphyKeyStatus(s)) => s.configured,
        other => panic!("expected GiphyKeyStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn giphy_key_status_follows_the_stored_key() {
    let store = Arc::new(MemoryStore::default());
    let mut o = test_options();
    o.secrets = store.clone();
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await;
    let mut c = d.client().await;
    assert!(!configured(&mut c).await, "nothing stored");
    store.set("giphy", "gk_status_1").unwrap();
    assert!(configured(&mut c).await);
    let reply = c.request(Command::GiphyKeyStatus).await;
    assert!(
        !format!("{reply:?}").contains("gk_status_1"),
        "the key is never sent"
    );
    store.set("giphy", "   ").unwrap();
    assert!(!configured(&mut c).await, "a blank key is no key");
    store.delete("giphy").unwrap();
    assert!(!configured(&mut c).await);
    d.stop().await;
}

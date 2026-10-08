//! `OpenWindow` reaches the connections subscribed to `window`, and only counts live ones.

mod common;

use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_protocol::{Command, Event, Reply, WindowTarget, topics};
use common::TestDaemon;

#[tokio::test]
async fn open_window_reaches_listening_windows() {
    let d = TestDaemon::start().await;
    let mut asker = d.client().await;
    assert_eq!(
        asker
            .request(Command::OpenWindow {
                target: WindowTarget::Home
            })
            .await
            .unwrap(),
        Reply::Delivered(0),
        "nobody listens yet"
    );

    let mut window = d.client().await;
    for _ in 0..2 {
        // Subscribing twice still counts this connection once.
        window
            .request(Command::Subscribe {
                topics: vec![topics::WINDOW.into()],
            })
            .await
            .unwrap();
    }
    let pr: PrRef = "acme/widgets#7".parse().unwrap();
    let target = WindowTarget::Review { pr };
    assert_eq!(
        asker
            .request(Command::OpenWindow {
                target: target.clone()
            })
            .await
            .unwrap(),
        Reply::Delivered(1)
    );
    let (topic, event) = tokio::time::timeout(common::EVENT_WAIT, window.next_event())
        .await
        .expect("event arrives")
        .unwrap();
    assert_eq!(topic, topics::WINDOW);
    assert_eq!(event, Event::WindowRequested { target });

    drop(window);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = asker
            .request(Command::OpenWindow {
                target: WindowTarget::Config,
            })
            .await
            .unwrap();
        if reply == Reply::Delivered(0) {
            break;
        }
        assert!(Instant::now() < deadline, "closed windows stop counting");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    d.stop().await;
}

//! A second launch hands its target to the open window through the daemon.

mod common;

use std::time::Duration;

use clusia_app::instance::hand_over;
use clusia_protocol::{Client, Command, Event, WindowTarget, topics};

#[tokio::test]
async fn forward_retries_until_delivered() {
    let d = common::Daemon::start().await;
    let mut asker = d.client().await;
    let socket = d.paths.socket();
    let window = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut w = Client::connect(&socket, "late-window").await.unwrap();
        w.request(Command::Subscribe {
            topics: vec![topics::WINDOW.into()],
        })
        .await
        .unwrap();
        w.next_event().await.unwrap()
    });
    let n = hand_over(
        &mut asker,
        &WindowTarget::Config,
        40,
        Duration::from_millis(50),
    )
    .await
    .unwrap();
    assert_eq!(n, 1);
    let (_, event) = window.await.unwrap();
    assert_eq!(
        event,
        Event::WindowRequested {
            target: WindowTarget::Config
        }
    );
    d.stop().await;
}

#[tokio::test]
async fn forward_gives_up_with_zero() {
    let d = common::Daemon::start().await;
    let mut c = d.client().await;
    let n = hand_over(&mut c, &WindowTarget::Home, 3, Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(n, 0);
    d.stop().await;
}

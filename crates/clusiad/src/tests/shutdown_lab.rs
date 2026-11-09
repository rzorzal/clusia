//! Nothing outlives its review, the app or the daemon: a review's close, publish and discard,
//! the last window going, a daemon shutdown, a daemon restart and the sweep each leave no
//! system behind.

use std::time::Duration;

use clusia_core::PrRef;
use clusia_core::system::{StopWhy, SystemLive, SystemRecord, SystemState};
use clusia_core::tests::TestsRecord;
use clusia_protocol::Outcome;
use clusia_store::{load_tests, save_tests};
use clusia_testrun::testkit::FakeReply;

use super::lab::{Lab, Machine, lab, pr};
use super::send_lab::{install, install_pr, teardown_rules};
use super::system_lab::system_of;
use super::*;
use crate::connection::Listener;
use crate::turns::Why;

fn two() -> Lab {
    lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7, 6],
        |x, _| teardown_rules(x),
    )
}

/// `text` with the plan's stand-in slug `acme-widgets-1a2b3c` replaced by the real one.
fn real(text: &str) -> String {
    text.replace(
        "acme-widgets-1a2b3c",
        &clusia_testrun::names::repo_slug("acme", "widgets"),
    )
}

/// The `docker` calls whose first argument is `verb`.
fn calls_of(lab: &Lab, verb: &str) -> Vec<Vec<String>> {
    lab.exec
        .calls()
        .iter()
        .map(|c| {
            c.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .filter(|a| a.first().is_some_and(|f| f == verb))
        .collect()
}

/// Every call, as its arguments joined by spaces.
fn ran(lab: &Lab) -> Vec<String> {
    lab.exec
        .calls()
        .iter()
        .map(|c| {
            c.args
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn state_of(lab: &Lab, n: u64) -> SystemState {
    lab.shared
        .tests
        .with(&pr(n), |e| e.system.as_ref().map(|s| s.state))
        .unwrap()
}

fn installed_two(lab: &Lab) {
    install(
        lab,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, Some(49_152))],
    );
    install(
        lab,
        6,
        SystemState::Running,
        true,
        &[("api", 8080, Some(49_153))],
    );
}

/// Lets spawned tasks run and the paused clock advance by `by`.
async fn pass(by: Duration) {
    tokio::time::advance(by).await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn a_system_stops_when_its_review_closes_publishes_or_discards() {
    // Close (`Why::User`), publish and discard (`Why::Ended`, then `forget`) are the three paths.
    for (why, forgets, state) in [
        (Why::User, false, SystemState::Running),
        (Why::Ended, true, SystemState::Running),
        (Why::Ended, true, SystemState::Unhealthy),
    ] {
        let lab = two();
        install(&lab, 7, state, false, &[("api", 8080, Some(49_152))]);
        let record = TestsRecord {
            stack: Some(real("clusia-acme-widgets-1a2b3c-7")),
            ..TestsRecord::default()
        };
        save_tests(&lab.shared.paths, &pr(7), &record).unwrap();
        let logs = lab.shared.paths.system_logs_for(&pr(7));
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("start.log"), "x").unwrap();

        stop_all(&lab.shared, &pr(7), why).await;

        // Awaited: everything has run by the time `stop_all` returns, and before the stack goes down.
        let ran = ran(&lab);
        let at = |needle: &str| {
            ran.iter()
                .position(|c| c.starts_with(needle))
                .unwrap_or_else(|| panic!("{needle} in {ran:?}"))
        };
        assert!(
            at(&real("stop -t 10 clusia-system-acme-widgets-1a2b3c-7"))
                < at(&real("rm -f clusia-system-acme-widgets-1a2b3c-7 "))
        );
        assert!(
            at("rm -f clusia-system-")
                < at(&real("network rm clusia-system-acme-widgets-1a2b3c-7-pub"))
        );
        assert!(
            at(&real("network rm clusia-system-acme-widgets-1a2b3c-7-net"))
                < at(&real("compose -p clusia-acme-widgets-1a2b3c-7 down"))
        );
        assert_eq!(
            state_of(&lab, 7),
            SystemState::Stopped {
                why: StopWhy::ReviewClosed
            }
        );
        assert_eq!(
            load_tests(&lab.shared.paths, &pr(7)).unwrap().system.live,
            None
        );

        if forgets {
            forget(&lab.shared, &pr(7));
            assert!(!lab.shared.paths.tests_file(&pr(7)).exists());
            assert!(
                !logs.exists(),
                "the system's log folder goes with the review"
            );
            assert!(lab.shared.tests.with(&pr(7), |e| e.system.is_none()));
        }
    }
}

#[tokio::test]
async fn forget_without_stop_all_still_removes_the_system() {
    let lab = two();
    install(
        &lab,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, Some(49_152))],
    );
    let stop = lab
        .shared
        .tests
        .with(&pr(7), |e| e.system.as_ref().unwrap().stop.clone());
    forget(&lab.shared, &pr(7));
    assert!(stop.fired(), "its streams end");
    let give_up = std::time::Instant::now() + Duration::from_secs(5);
    while calls_of(&lab, "network").is_empty() && std::time::Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(calls_of(&lab, "rm").len(), 1, "removed in the background");
    assert!(calls_of(&lab, "network").iter().any(|c| c[1] == "rm"));
}

#[tokio::test]
async fn forget_unlinks_a_log_folder_link_and_never_follows_it() {
    let lab = two();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("keep.txt"), "x").unwrap();
    let logs = lab.shared.paths.system_logs_for(&pr(7));
    std::fs::create_dir_all(logs.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), &logs).unwrap();
    forget(&lab.shared, &pr(7));
    assert!(
        std::fs::symlink_metadata(&logs).is_err(),
        "the link is gone"
    );
    assert!(
        elsewhere.path().join("keep.txt").exists(),
        "its target is untouched"
    );
}

#[tokio::test(start_paused = true)]
async fn the_last_window_closing_stops_app_systems_after_the_grace() {
    let lab = two();
    installed_two(&lab);
    let first = Listener::window(&lab.shared);
    let second = Listener::window(&lab.shared);
    drop(first);
    pass(Duration::from_secs(60)).await;
    assert_eq!(
        state_of(&lab, 7),
        SystemState::Running,
        "a window is still open"
    );
    drop(second);
    pass(Duration::from_millis(9_900)).await;
    assert_eq!(state_of(&lab, 7), SystemState::Running, "inside the grace");
    assert!(calls_of(&lab, "stop").is_empty());
    pass(Duration::from_millis(200)).await;
    assert_eq!(
        state_of(&lab, 7),
        SystemState::Stopped {
            why: StopWhy::AppQuit
        }
    );
    assert_eq!(calls_of(&lab, "stop").len(), 1, "only the app's system");
    assert_eq!(
        state_of(&lab, 6),
        SystemState::Running,
        "the kept one stays"
    );
    // Nothing re-arms: an hour later, not one more docker call.
    let before = lab.exec.calls().len();
    pass(Duration::from_secs(3_600)).await;
    assert_eq!(lab.exec.calls().len(), before);
}

#[tokio::test(start_paused = true)]
async fn a_relaunch_inside_the_grace_keeps_the_system() {
    let lab = two();
    installed_two(&lab);
    drop(Listener::window(&lab.shared));
    pass(Duration::from_secs(5)).await;
    let relaunched = Listener::window(&lab.shared);
    pass(Duration::from_secs(30)).await;
    assert_eq!(state_of(&lab, 7), SystemState::Running);
    assert!(calls_of(&lab, "stop").is_empty());
    // The grace counts from the last window gone, not the first: gone, back within the grace,
    // gone again 8 s later; the task of the earlier "gone" finds a newer one and stops nothing.
    drop(relaunched);
    pass(Duration::from_secs(8)).await;
    let again = Listener::window(&lab.shared);
    drop(again);
    pass(Duration::from_millis(9_900)).await;
    assert_eq!(
        state_of(&lab, 7),
        SystemState::Running,
        "the earlier task found a newer 'last window gone'"
    );
    pass(Duration::from_millis(200)).await;
    assert_eq!(
        state_of(&lab, 7),
        SystemState::Stopped {
            why: StopWhy::AppQuit
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_kept_system_outlives_the_app() {
    let lab = two();
    install(
        &lab,
        6,
        SystemState::Running,
        true,
        &[("api", 8080, Some(49_153))],
    );
    drop(Listener::window(&lab.shared));
    pass(Duration::from_secs(600)).await;
    assert_eq!(state_of(&lab, 6), SystemState::Running);
    assert!(calls_of(&lab, "stop").is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_app_that_quit_while_the_system_started_still_stops_it() {
    let lab = two();
    install(
        &lab,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, Some(49_152))],
    );
    system::on_settled(&lab.shared);
    pass(Duration::from_secs(11)).await;
    assert_eq!(
        state_of(&lab, 7),
        SystemState::Stopped {
            why: StopWhy::AppQuit
        }
    );

    // With a window open, a system that settles is left alone.
    let lab = two();
    install(
        &lab,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, Some(49_152))],
    );
    let _window = Listener::window(&lab.shared);
    system::on_settled(&lab.shared);
    pass(Duration::from_secs(60)).await;
    assert_eq!(state_of(&lab, 7), SystemState::Running);
}

#[tokio::test]
async fn the_quit_grace_is_ten_seconds_and_settable() {
    let lab = two();
    assert_eq!(lab.shared.tests.quit_grace(), Duration::from_secs(10));
    lab.shared.tests.set_quit_grace(Duration::from_millis(30));
    assert_eq!(lab.shared.tests.quit_grace(), Duration::from_millis(30));
}

#[tokio::test]
async fn a_daemon_shutdown_stops_every_system_before_the_stacks_go_down() {
    let lab = two();
    installed_two(&lab);
    for n in [7, 6] {
        let record = TestsRecord {
            stack: Some(real(&format!("clusia-acme-widgets-1a2b3c-{n}"))),
            ..TestsRecord::default()
        };
        save_tests(&lab.shared.paths, &pr(n), &record).unwrap();
    }
    shutdown(&lab.shared).await;
    assert_eq!(
        state_of(&lab, 7),
        SystemState::Stopped {
            why: StopWhy::Shutdown
        }
    );
    assert_eq!(
        state_of(&lab, 6),
        SystemState::Stopped {
            why: StopWhy::Shutdown
        },
        "kept systems stop with the daemon"
    );
    let ran = ran(&lab);
    let last_net = ran
        .iter()
        .rposition(|c| c.starts_with("network rm"))
        .unwrap();
    let first_down = ran
        .iter()
        .position(|c| c.starts_with("compose -p"))
        .unwrap();
    assert!(last_net < first_down, "{ran:?}");
    // After the shutdown no window event wakes anything.
    lab.shared.tests.set_quit_grace(Duration::from_millis(10));
    drop(Listener::window(&lab.shared));
    let before = lab.exec.calls().len();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(lab.exec.calls().len(), before);
}

#[tokio::test]
async fn the_sweep_removes_orphan_systems_and_keeps_live_ones() {
    let listing = real(
        "clusia-system-acme-widgets-1a2b3c-7 \n\
                   clusia-system-acme-widgets-1a2b3c-7-fwd \n\
                   clusia-system-acme-widgets-1a2b3c-9 \n\
                   clusia-system-acme-widgets-1a2b3c-9-fwd \n",
    );
    let networks = real(
        "clusia-system-acme-widgets-1a2b3c-7-pub\nclusia-system-acme-widgets-1a2b3c-7-net\n\
                    clusia-system-acme-widgets-1a2b3c-9-pub\nclusia-system-acme-widgets-1a2b3c-9-net\n",
    );
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |x, _| {
            x.on("docker", &["ps", "-a", "--filter"], FakeReply::ok(listing));
            x.on("docker", &["network", "ls"], FakeReply::ok(networks));
            teardown_rules(x);
        },
    );
    install(
        &lab,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, Some(49_152))],
    );
    save_tests(&lab.shared.paths, &pr(7), &TestsRecord::default()).unwrap();
    let kept_logs = lab.shared.paths.system_logs_for(&pr(7));
    let gone_logs = lab.shared.paths.system_logs_for(&pr(9));
    for dir in [&kept_logs, &gone_logs] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("start.log"), "x").unwrap();
    }

    sweep(&lab.shared, crate::sync::now_unix() + 1_000).await;

    let rm = calls_of(&lab, "rm");
    assert_eq!(rm.len(), 1, "{rm:?}");
    assert_eq!(
        rm[0],
        [
            "rm",
            "-f",
            real("clusia-system-acme-widgets-1a2b3c-9").as_str(),
            real("clusia-system-acme-widgets-1a2b3c-9-fwd").as_str()
        ]
    );
    let net_rm: Vec<Vec<String>> = calls_of(&lab, "network")
        .into_iter()
        .filter(|c| c[1] == "rm")
        .collect();
    assert_eq!(
        net_rm,
        vec![vec![
            "network",
            "rm",
            real("clusia-system-acme-widgets-1a2b3c-9-pub").as_str(),
            real("clusia-system-acme-widgets-1a2b3c-9-net").as_str()
        ]]
    );
    assert!(kept_logs.exists(), "a review with a record keeps its logs");
    assert!(
        !gone_logs.exists(),
        "a log folder whose record is gone is removed"
    );
}

#[tokio::test]
async fn a_daemon_restart_leaves_no_system_and_says_so() {
    let listing =
        real("clusia-system-acme-widgets-1a2b3c-7 \nclusia-system-acme-widgets-1a2b3c-7-fwd \n");
    let networks =
        real("clusia-system-acme-widgets-1a2b3c-7-pub\nclusia-system-acme-widgets-1a2b3c-7-net\n");
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |x, _| {
            x.on("docker", &["ps", "-a", "--filter"], FakeReply::ok(listing));
            x.on("docker", &["network", "ls"], FakeReply::ok(networks));
            teardown_rules(x);
        },
    );
    // The record of the process that died says a system was live; this process has none.
    let record = TestsRecord {
        system: SystemRecord {
            network: true,
            live: Some(SystemLive {
                head: "h1".into(),
                at: 1_762_300_000,
                keep: true,
            }),
        },
        ..TestsRecord::default()
    };
    save_tests(&lab.shared.paths, &pr(7), &record).unwrap();

    let view = system_of(system(&lab.shared, &pr(7)).await);
    assert_eq!(
        view.state,
        SystemState::Stopped {
            why: StopWhy::DaemonRestart
        }
    );
    assert_eq!(
        clusia_core::system::stop_text(StopWhy::DaemonRestart),
        "Stopped when Clúsia restarted"
    );
    assert!(
        !view.keep
            && view.running_since.is_none()
            && view.ports.is_empty()
            && view.processes.is_empty()
    );
    assert!(view.network, "the reviewer's network choice survives");

    crate::retention::sweep(&lab.shared).await;
    assert_eq!(
        calls_of(&lab, "rm")[0],
        [
            "rm",
            "-f",
            real("clusia-system-acme-widgets-1a2b3c-7").as_str(),
            real("clusia-system-acme-widgets-1a2b3c-7-fwd").as_str()
        ]
    );
    assert!(
        calls_of(&lab, "network")
            .iter()
            .any(|c| c[1] == "rm" && c[2].ends_with("-7-pub"))
    );
    for started in ["create", "start"] {
        assert!(
            calls_of(&lab, started).is_empty(),
            "nothing restarts on its own: {started}"
        );
    }
    forget(&lab.shared, &pr(7));
    assert!(
        load_tests(&lab.shared.paths, &pr(7))
            .unwrap_or_default()
            .system
            .live
            .is_none()
    );
}

#[tokio::test]
async fn live_names_cover_a_live_system_and_a_stopped_one_has_none() {
    let lab = two();
    install(
        &lab,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, Some(49_152))],
    );
    install(
        &lab,
        6,
        SystemState::Stopped {
            why: StopWhy::Reviewer,
        },
        false,
        &[],
    );
    let names = env::live_names(&lab.shared);
    for suffix in ["", "-fwd", "-pub", "-net"] {
        assert!(
            names.contains(&real(&format!(
                "clusia-system-acme-widgets-1a2b3c-7{suffix}"
            ))),
            "{suffix}"
        );
        assert!(
            !names.contains(&real(&format!(
                "clusia-system-acme-widgets-1a2b3c-6{suffix}"
            ))),
            "{suffix}"
        );
    }
}

#[tokio::test]
async fn clean_up_refuses_while_a_live_system_belongs_to_the_repository() {
    let lab = two();
    install(
        &lab,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, Some(49_152))],
    );
    let refused = super::lab::refused;
    assert_eq!(
        refused(clean_up(&lab.shared, Some("acme/widgets".into())).await),
        "Stop the system of acme/widgets#7 first"
    );
    assert!(
        calls_of(&lab, "rm").is_empty() && calls_of(&lab, "image").is_empty(),
        "nothing was removed"
    );
    // Every repository with a cache folder: acme/widgets has one, so it refuses too.
    std::fs::create_dir_all(lab.shared.paths.tests_dir().join("cache/acme~widgets")).unwrap();
    assert_eq!(
        refused(clean_up(&lab.shared, None).await),
        "Stop the system of acme/widgets#7 first"
    );
    // Another repository's live system does not block this one, and a stopped system never blocks.
    let other: PrRef = "octo/gizmo#2".parse().unwrap();
    lab.shared.tests.with(&pr(7), |e| {
        e.system.as_mut().unwrap().state = SystemState::Stopped {
            why: StopWhy::Reviewer,
        }
    });
    install_pr(
        &lab,
        &other,
        SystemState::Running,
        false,
        &[("api", 8080, Some(49_160))],
    );
    assert!(matches!(
        clean_up(&lab.shared, Some("acme/widgets".into())).await,
        Outcome::Ok(_)
    ));
}

#[tokio::test]
async fn a_teardown_returns_with_the_log_files_complete() {
    use clusia_testrun::exec::Stream;

    // One line now, then nothing until the stop: the line sits in the file's buffer.
    let (lab, _api) = super::system_lab::system_lab_without_health_with(vec![], |x| {
        x.on(
            "docker",
            &["logs"],
            FakeReply::timed(
                vec![
                    (Duration::ZERO, Stream::Out, "ready on 8080\n".to_string()),
                    (
                        Duration::from_secs(3_600),
                        Stream::Out,
                        "never\n".to_string(),
                    ),
                ],
                0,
            ),
        );
    })
    .await;
    super::system_lab::start(&lab, false).await;
    super::system_lab::until_state(&lab, |v| {
        v.state == SystemState::Running && v.processes.iter().any(|p| p.last >= 1)
    })
    .await;

    system::teardown(&lab.shared, &pr(7), StopWhy::Reviewer).await;

    let log = lab.shared.paths.system_logs_for(&pr(7)).join("start.log");
    assert_eq!(std::fs::read_to_string(log).unwrap(), "ready on 8080\n");
}

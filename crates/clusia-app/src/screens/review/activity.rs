//! Where the agent is working for a review: its chat turn, a security check or audits run, the
//! diagrams' plan and drawing, and a revalidation round. Every indicator (the live row of the
//! chat, the tab marks, the status bar) reads these from the state the window already holds, so
//! they start and stop together with the events that change it.

use clusia_core::PrRef;
use clusia_core::checks::CheckKind;
use clusia_core::revalidation::RoundState;
use clusia_protocol::SessionStateKind;

use super::agent::model::{ChatModel, Chats};
use super::checks::{Checks, ChecksModel};
use super::diagrams::{Diagrams, DiagramsModel};
use super::revalidation::{RevalidationModel, Revalidations};
use super::shell::{agent_status, drawing_status};

/// What a tab label carries in place of a count while its run goes on; the tab strip draws a
/// spinner for it.
pub const MARK: &str = "●";

/// `7s`, `1m 05s`, `1h 02m`: how long a turn has run.
pub fn elapsed_label(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// One review's state of every kind of agent work.
#[derive(Debug, Clone, Copy, Default)]
pub struct Work<'a> {
    pub chat: Option<&'a ChatModel>,
    pub checks: Option<&'a ChecksModel>,
    pub diagrams: Option<&'a DiagramsModel>,
    pub revalidation: Option<&'a RevalidationModel>,
}

impl<'a> Work<'a> {
    pub fn of(
        pr: &PrRef,
        chats: &'a Chats,
        checks: &'a Checks,
        diagrams: &'a Diagrams,
        revalidations: &'a Revalidations,
    ) -> Self {
        Self {
            chat: chats.0.get(pr),
            checks: checks.0.get(pr),
            diagrams: diagrams.0.get(pr),
            revalidation: revalidations.0.get(pr),
        }
    }

    /// A chat turn runs or waits for its turn.
    pub fn turn(&self) -> bool {
        self.chat.is_some_and(ChatModel::busy)
    }

    /// The check of `kind` waits for a place or runs.
    pub fn check(&self, kind: CheckKind) -> bool {
        self.checks.is_some_and(|m| m.running(kind))
    }

    /// The diagrams are being planned or drawn.
    pub fn drawing(&self) -> bool {
        self.diagrams.is_some_and(DiagramsModel::busy)
    }

    /// A revalidation round is running.
    pub fn revalidating(&self) -> bool {
        self.revalidation
            .and_then(|m| m.round.as_ref())
            .is_some_and(|r| matches!(r.state, RoundState::Running { .. }))
    }

    /// Any agent work runs for the review.
    pub fn any(&self) -> bool {
        self.turn()
            || self.drawing()
            || self.revalidating()
            || self.check(CheckKind::Security)
            || self.check(CheckKind::Audit)
    }

    /// The status bar's agent text and whether its dot is lit, with what runs named. A turn
    /// says what it is; a run that has no turn of its own in the chat names itself.
    pub fn status(&self, name: &str) -> (String, bool) {
        if let Some(drawing) = self.diagrams.and_then(|m| drawing_status(&m.state, name)) {
            return (drawing, true);
        }
        let state = self.chat.map_or(SessionStateKind::None, |c| c.state);
        if self.turn() {
            return agent_status(state, name);
        }
        let running = if self.check(CheckKind::Security) {
            "running the security check"
        } else if self.check(CheckKind::Audit) {
            "running the audits"
        } else if self.revalidating() {
            "revalidating your comments"
        } else {
            return agent_status(state, name);
        };
        (format!("{name} · {running}…"), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_protocol::{CheckState, DiagramsState};

    use crate::bridge::{AgentTell, ChecksTell, DiagramsTell};

    fn pr() -> PrRef {
        PrRef {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        }
    }

    fn state(chats: &mut Chats, state: SessionStateKind) {
        chats.entry(&pr()).state = state;
    }

    #[test]
    fn elapsed_time_reads_in_seconds_minutes_and_hours() {
        assert_eq!(elapsed_label(0), "0s");
        assert_eq!(elapsed_label(59), "59s");
        assert_eq!(elapsed_label(60), "1m 00s");
        assert_eq!(elapsed_label(125), "2m 05s");
        assert_eq!(elapsed_label(3600 + 120), "1h 02m");
    }

    #[test]
    fn a_turn_is_work_while_it_runs_or_waits_and_not_after() {
        let mut chats = Chats::default();
        let (checks, diagrams, revals) = Default::default();
        for (s, want) in [
            (SessionStateKind::None, false),
            (SessionStateKind::Ready, false),
            (SessionStateKind::Queued, true),
            (SessionStateKind::Running, true),
        ] {
            state(&mut chats, s);
            let work = Work::of(&pr(), &chats, &checks, &diagrams, &revals);
            assert_eq!((work.turn(), work.any()), (want, want), "{s:?}");
        }
    }

    #[test]
    fn a_chat_that_ends_by_done_error_or_stop_is_no_longer_work() {
        let mut chats = Chats::default();
        let (checks, diagrams, revals) = Default::default();
        for end in [
            AgentTell::Done {
                pr: pr(),
                turn: 1,
                duration_ms: 5,
            },
            AgentTell::Error {
                pr: pr(),
                turn: 1,
                kind: clusia_protocol::AgentErrorKind::Crashed,
                message: "boom".into(),
            },
        ] {
            state(&mut chats, SessionStateKind::Running);
            chats.apply(&end);
            // The daemon tells the state after the end of the turn, a stop included.
            chats.apply(&AgentTell::State {
                pr: pr(),
                state: SessionStateKind::Ready,
            });
            assert!(!Work::of(&pr(), &chats, &checks, &diagrams, &revals).any());
        }
    }

    #[test]
    fn a_check_is_work_while_it_waits_or_runs_and_not_when_done_failed_or_stopped() {
        let (chats, diagrams, revals) = Default::default();
        let mut checks = Checks::default();
        for (s, want) in [
            (CheckState::Waiting, true),
            (CheckState::Running { activity: None }, true),
            (CheckState::Done, false),
            (
                CheckState::Failed {
                    message: "x".into(),
                },
                false,
            ),
            (CheckState::NotRun, false),
            (CheckState::Stale, false),
        ] {
            checks.apply(&ChecksTell::State {
                pr: pr(),
                kind: CheckKind::Security,
                state: s.clone(),
            });
            let work = Work::of(&pr(), &chats, &checks, &diagrams, &revals);
            assert_eq!(work.check(CheckKind::Security), want, "{s:?}");
            assert!(
                !work.check(CheckKind::Audit),
                "the other kind is not touched"
            );
            assert_eq!(work.any(), want);
        }
    }

    #[test]
    fn the_diagrams_are_work_while_planned_or_drawn_and_not_idle_or_failed() {
        let (chats, checks, revals) = Default::default();
        let mut diagrams = Diagrams::default();
        for (s, want) in [
            (DiagramsState::Planning, true),
            (
                DiagramsState::Drawing {
                    done: 1,
                    total: 3,
                    key: "flow".into(),
                },
                true,
            ),
            (DiagramsState::Idle, false),
            (
                DiagramsState::Failed {
                    message: "x".into(),
                },
                false,
            ),
        ] {
            diagrams.apply(&DiagramsTell::State {
                pr: pr(),
                state: s.clone(),
            });
            let work = Work::of(&pr(), &chats, &checks, &diagrams, &revals);
            assert_eq!(work.drawing(), want, "{s:?}");
            assert_eq!(work.any(), want);
        }
    }

    #[test]
    fn a_round_is_work_only_while_it_runs() {
        let (chats, checks, diagrams) = Default::default();
        let mut revals = Revalidations::default();
        let mut app = crate::testing::app(crate::snapshot::Snapshot::default());
        let pr = crate::testing::open_ready(&mut app, false);
        let mut review = crate::testing::ready(&app, &pr).view.review.clone();
        for (s, want) in [
            (RoundState::Running { activity: None }, true),
            (RoundState::Scheduled { at: 5 }, false),
            (RoundState::Waiting, false),
            (RoundState::Done, false),
            (
                RoundState::Failed {
                    message: "x".into(),
                },
                false,
            ),
        ] {
            revals.apply(&crate::bridge::RevalidationTell::Round {
                pr: pr.clone(),
                round: Some(crate::testing::demo_round(&mut review, s.clone())),
            });
            let work = Work::of(&pr, &chats, &checks, &diagrams, &revals);
            assert_eq!(work.revalidating(), want, "{s:?}");
            assert_eq!(work.any(), want);
        }
    }

    #[test]
    fn the_status_names_what_runs_and_agrees_with_the_marks() {
        let mut chats = Chats::default();
        let (mut checks, mut diagrams, revals) =
            (Checks::default(), Diagrams::default(), Default::default());
        state(&mut chats, SessionStateKind::Ready);
        let idle = Work::of(&pr(), &chats, &checks, &diagrams, &revals).status("Claude Code");
        assert_eq!(idle, ("Claude Code · ready".to_string(), true));

        checks.apply(&ChecksTell::State {
            pr: pr(),
            kind: CheckKind::Audit,
            state: CheckState::Running { activity: None },
        });
        let work = Work::of(&pr(), &chats, &checks, &diagrams, &revals);
        assert_eq!(
            work.status("Claude Code"),
            ("Claude Code · running the audits…".to_string(), true)
        );

        state(&mut chats, SessionStateKind::Running);
        let work = Work::of(&pr(), &chats, &checks, &diagrams, &revals);
        assert_eq!(work.status("Claude Code").0, "Claude Code · thinking…");

        diagrams.apply(&DiagramsTell::State {
            pr: pr(),
            state: DiagramsState::Drawing {
                done: 2,
                total: 5,
                key: "flow".into(),
            },
        });
        let work = Work::of(&pr(), &chats, &checks, &diagrams, &revals);
        assert_eq!(
            work.status("Claude Code").0,
            "Claude Code · drawing diagrams 3 of 5"
        );
        assert!(work.any());
    }
}

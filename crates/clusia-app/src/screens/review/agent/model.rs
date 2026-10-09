//! The chat of each review: what the agent said, did and suggested, built from the daemon's
//! agent events and from its log.

use std::collections::HashMap;

use bevy::prelude::*;
use clusia_core::PrRef;
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, PermissionOutcome, SessionStateKind, Suggestion,
};

use crate::bridge::AgentTell;

pub const DEFAULT_WIDTH: f32 = 420.0;
pub const MIN_WIDTH: f32 = 320.0;
pub const MAX_WIDTH: f32 = 640.0;

/// What a fenced block that starts a suggestion begins with.
const SUGGESTION_FENCE: &str = "```clusia-suggestion";

/// The two tabs of the review's right column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelTab {
    Agent,
    #[default]
    Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionState {
    Waiting,
    Accepted,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatLine {
    /// What the user asked.
    Me(String),
    /// The agent's answer as it arrived (markdown, suggestion blocks still in it).
    Text(String),
    /// `Read src/auth/store.rs`
    Tool(String),
    /// A tool the agent was not allowed to use.
    Denied {
        tool: String,
        detail: String,
    },
    /// A permission request that ended: what was asked and how it ended.
    Permission {
        tool: String,
        summary: String,
        outcome: PermissionOutcome,
    },
    Error(String),
    Suggestion {
        suggestion: Suggestion,
        state: SuggestionState,
    },
}

/// One review's chat and how its column is shown.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatModel {
    pub lines: Vec<ChatLine>,
    pub state: SessionStateKind,
    /// The right column of the review is shown (⌘\ or the status bar hides it).
    pub open: bool,
    /// Which of the column's two tabs is selected.
    pub tab: PanelTab,
    /// The user chose a tab, so neither the harness nor the agent speaking picks one again.
    pub tab_set: bool,
    pub width: f32,
    /// The log had entries when the review opened.
    pub resumed: bool,
    /// The log was asked for.
    pub log_asked: bool,
    /// A question to put in the input (from "Ask the agent about this line"); the panel
    /// does it on the next frame.
    pub prefill: Option<String>,
    /// The rules granted for this review, as the daemon told them (`Bash(cargo test:*)`, `Edit`).
    pub rules: Vec<String>,
    /// The turn whose answer the last `Text` line is still receiving.
    streaming: Option<u64>,
    /// The turn that was still running when the log was read. The daemon logs an answer's
    /// text only when the answer is cut by a tool, a suggestion or the end of the turn, so
    /// that log lacked what had streamed so far; once this turn ends the log has all of it.
    unfinished: Option<u64>,
    /// The turn the log was last read again for: a turn whose end never reaches the log (cut
    /// by the end of its review) is not read again a second time.
    asked_again: Option<u64>,
    /// Parallel to `lines`: the lines only this window has, which the log does not replay (a
    /// refused send and why, a handled suggestion the log leaves out).
    local: Vec<bool>,
}

impl Default for ChatModel {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            state: SessionStateKind::None,
            open: true,
            tab: PanelTab::Draft,
            tab_set: false,
            width: DEFAULT_WIDTH,
            resumed: false,
            log_asked: false,
            prefill: None,
            rules: Vec::new(),
            streaming: None,
            unfinished: None,
            asked_again: None,
            local: Vec::new(),
        }
    }
}

impl ChatModel {
    pub fn waiting(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| {
                matches!(
                    l,
                    ChatLine::Suggestion {
                        state: SuggestionState::Waiting,
                        ..
                    }
                )
            })
            .count()
    }

    /// A turn is running or waiting for its turn.
    pub fn busy(&self) -> bool {
        matches!(
            self.state,
            SessionStateKind::Running | SessionStateKind::Queued
        )
    }

    /// Hides or shows the right column.
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Puts `prompt` in the input, on the Agent tab, which the user is then on by choice.
    pub fn ask_about(&mut self, prompt: String) {
        self.prefill = Some(prompt);
        self.show(PanelTab::Agent);
        self.open = true;
    }

    /// Selects a tab on the user's behalf.
    pub fn show(&mut self, tab: PanelTab) {
        self.tab = tab;
        self.tab_set = true;
    }

    pub fn resize(&mut self, dx: f32) {
        self.width = resized(self.width, dx);
    }

    fn push(&mut self, line: ChatLine) {
        self.streaming = None;
        self.sync_local();
        self.lines.push(line);
        self.local.push(false);
    }

    /// `lines` is public; a line added to it from outside counts as the log's.
    fn sync_local(&mut self) {
        self.local.resize(self.lines.len(), false);
    }

    /// A line the log will never have.
    fn push_local(&mut self, line: ChatLine) {
        self.push(line);
        if let Some(last) = self.local.last_mut() {
            *last = true;
        }
    }

    pub fn push_me(&mut self, text: String) {
        self.push(ChatLine::Me(text));
    }

    pub fn push_error(&mut self, message: String) {
        self.push_local(ChatLine::Error(message));
    }

    /// Sets what became of a suggestion. A handled suggestion leaves the log's replay, so its
    /// line is kept as this window's own.
    pub fn mark(&mut self, id: &str, to: SuggestionState) {
        self.sync_local();
        for (line, local) in self.lines.iter_mut().zip(self.local.iter_mut()) {
            if let ChatLine::Suggestion { suggestion, state } = line
                && suggestion.id == id
            {
                *state = to;
                if to != SuggestionState::Waiting {
                    *local = true;
                }
            }
        }
    }

    /// A send was refused: the question never reached the log either.
    fn refused(&mut self, message: String) {
        self.sync_local();
        if let Some(at) = self
            .lines
            .iter()
            .rposition(|l| matches!(l, ChatLine::Me(_)))
        {
            self.local[at] = true;
        }
        self.push_local(ChatLine::Error(message));
    }

    /// The agent speaking selects its tab, unless the user already chose one.
    fn spoke(&mut self) {
        if !self.tab_set {
            self.tab = PanelTab::Agent;
        }
    }

    /// Applies `tell`; true when the log must be asked again (see `unfinished`).
    fn apply(&mut self, tell: &AgentTell) -> bool {
        match tell {
            AgentTell::Chunk { turn, text, .. } => {
                self.spoke();
                match (self.lines.last_mut(), self.streaming) {
                    (Some(ChatLine::Text(shown)), Some(open)) if open == *turn => {
                        shown.push_str(text);
                    }
                    _ => {
                        self.lines.push(ChatLine::Text(text.clone()));
                        self.sync_local();
                    }
                }
                self.streaming = Some(*turn);
            }
            AgentTell::ToolUse { summary, .. } => {
                self.spoke();
                self.push(ChatLine::Tool(summary.clone()));
            }
            AgentTell::Denied { tool, detail, .. } => {
                self.spoke();
                self.push(ChatLine::Denied {
                    tool: tool.clone(),
                    detail: detail.clone(),
                });
            }
            AgentTell::Suggestion { suggestion, .. } => {
                self.spoke();
                self.push(ChatLine::Suggestion {
                    suggestion: suggestion.clone(),
                    state: SuggestionState::Waiting,
                });
            }
            AgentTell::Done { turn, .. } => {
                self.streaming = None;
                return self.ended(*turn);
            }
            AgentTell::Error {
                turn,
                kind,
                message,
                ..
            } => {
                self.spoke();
                self.push(ChatLine::Error(error_text(*kind, message)));
                return self.ended(*turn);
            }
            AgentTell::State { state, .. } => {
                self.state = *state;
                // The turn a replay caught running is over even when its `Done` was missed
                // (it came while the log was being read): read the log again for the rest.
                if matches!(state, SessionStateKind::Ready | SessionStateKind::None)
                    && let Some(turn) = self.unfinished.take()
                {
                    self.asked_again = Some(turn);
                    return true;
                }
            }
            AgentTell::Refused { message, .. } => self.refused(message.clone()),
            AgentTell::Handled { id, accepted, .. } => self.mark(
                id,
                if *accepted {
                    SuggestionState::Accepted
                } else {
                    SuggestionState::Dismissed
                },
            ),
        }
        false
    }

    /// A permission request of this review ended. The log has the same line, so it is the log's.
    fn permission(&mut self, tool: &str, summary: &str, outcome: PermissionOutcome) {
        self.spoke();
        self.push(ChatLine::Permission {
            tool: tool.to_string(),
            summary: summary.to_string(),
            outcome,
        });
    }

    /// `turn` ended: true when it is the turn the last replay caught running.
    fn ended(&mut self, turn: u64) -> bool {
        if self.unfinished == Some(turn) {
            self.unfinished = None;
            self.asked_again = Some(turn);
            return true;
        }
        false
    }

    /// Replaces the transcript with the log, keeping this window's own lines after the log line
    /// they followed. A turn still running when it was read lacks the text streamed since its
    /// last tool, suggestion or denial, so it is remembered in `unfinished` to read the log
    /// again when it ends. The state is not in the log: the daemon tells it after the log.
    fn replay(&mut self, entries: &[AgentLogEntry]) {
        let mut lines: Vec<ChatLine> = Vec::new();
        let mut streaming: Option<u64> = None;
        for entry in entries {
            if let AgentLogEntry::Text { turn, text, .. } = entry {
                match (lines.last_mut(), streaming) {
                    (Some(ChatLine::Text(shown)), Some(open)) if open == *turn => {
                        shown.push_str(text);
                    }
                    _ => lines.push(ChatLine::Text(text.clone())),
                }
                streaming = Some(*turn);
                continue;
            }
            streaming = None;
            match entry {
                AgentLogEntry::User { text, .. } => lines.push(ChatLine::Me(text.clone())),
                AgentLogEntry::ToolUse { summary, .. } => {
                    lines.push(ChatLine::Tool(summary.clone()));
                }
                AgentLogEntry::Denied { tool, detail, .. } => lines.push(ChatLine::Denied {
                    tool: tool.clone(),
                    detail: detail.clone(),
                }),
                AgentLogEntry::Suggestion { suggestion, .. } => lines.push(ChatLine::Suggestion {
                    suggestion: suggestion.clone(),
                    state: SuggestionState::Waiting,
                }),
                AgentLogEntry::Error { kind, message, .. } => {
                    lines.push(ChatLine::Error(error_text(*kind, message)));
                }
                AgentLogEntry::Permission {
                    tool,
                    summary,
                    outcome,
                    ..
                } => lines.push(ChatLine::Permission {
                    tool: tool.clone(),
                    summary: summary.clone(),
                    outcome: *outcome,
                }),
                AgentLogEntry::Done { .. } | AgentLogEntry::Text { .. } => {}
            }
        }
        let unfinished = match entries.last() {
            None | Some(AgentLogEntry::Done { .. }) | Some(AgentLogEntry::Error { .. }) => None,
            Some(
                AgentLogEntry::User { turn, .. }
                | AgentLogEntry::Text { turn, .. }
                | AgentLogEntry::ToolUse { turn, .. }
                | AgentLogEntry::Denied { turn, .. }
                | AgentLogEntry::Suggestion { turn, .. }
                | AgentLogEntry::Permission { turn, .. },
            ) => Some(*turn),
        };
        self.unfinished = unfinished.filter(|turn| self.asked_again != Some(*turn));
        // Each kept line goes back after as many log lines as it followed before.
        self.sync_local();
        let mut kept: Vec<(usize, ChatLine)> = Vec::new();
        let mut from_log = 0;
        for (line, local) in self.lines.drain(..).zip(self.local.drain(..)) {
            if local {
                kept.push((from_log, line));
            } else {
                from_log += 1;
            }
        }
        let mut merged = Vec::with_capacity(lines.len() + kept.len());
        let mut flags = Vec::with_capacity(merged.capacity());
        let mut kept = kept.into_iter().peekable();
        for (at, line) in lines.into_iter().enumerate() {
            while let Some((_, own)) = kept.next_if(|(after, _)| *after <= at) {
                merged.push(own);
                flags.push(true);
            }
            merged.push(line);
            flags.push(false);
        }
        for (_, own) in kept {
            merged.push(own);
            flags.push(true);
        }
        if flags.last() == Some(&true) {
            streaming = None;
        }
        self.lines = merged;
        self.local = flags;
        self.streaming = streaming;
        self.resumed = !entries.is_empty();
    }
}

/// The daemon's message, or the kind's default one when it sent none.
fn error_text(kind: AgentErrorKind, message: &str) -> String {
    if message.trim().is_empty() {
        kind.to_string()
    } else {
        message.to_string()
    }
}

/// The column's width after dragging its left edge by `dx` (a drag to the left is negative and
/// widens it).
pub fn resized(width: f32, dx: f32) -> f32 {
    (width - dx).clamp(MIN_WIDTH, MAX_WIDTH)
}

/// `src/auth/refresh.rs:44` as the draft cards show it: `refresh.rs:44`, `refresh.rs:40–44`.
pub fn place_of(s: &Suggestion) -> String {
    let name = s.file.rsplit('/').next().unwrap_or(&s.file);
    match (s.line, s.start_line, s.end_line) {
        (Some(line), _, _) => format!("{name}:{line}"),
        (None, Some(from), Some(to)) => format!("{name}:{from}–{to}"),
        _ => name.to_string(),
    }
}

/// The answer as the chat shows it: the valid suggestion blocks are cards of their own, and a
/// block that is still arriving stays hidden until it is whole.
pub fn display_text(raw: &str) -> String {
    let (shown, _) = clusia_harness::extract_suggestions(raw);
    let unfinished = shown.find(SUGGESTION_FENCE).filter(|at| {
        let after = &shown[at + SUGGESTION_FENCE.len()..];
        !after.contains("```")
    });
    match unfinished {
        Some(at) => shown[..at].trim().to_string(),
        None => shown.trim().to_string(),
    }
}

/// Every review's chat.
#[derive(Resource, Debug, Default)]
pub struct Chats(pub HashMap<PrRef, ChatModel>);

impl Chats {
    pub fn entry(&mut self, pr: &PrRef) -> &mut ChatModel {
        self.0.entry(pr.clone()).or_default()
    }

    pub fn state(&self, pr: &PrRef) -> SessionStateKind {
        self.0.get(pr).map_or(SessionStateKind::None, |c| c.state)
    }

    /// Applies an agent tell to its review's chat; a review without a chat (not open here, or
    /// still loading) ignores it, and its log carries the same events. True when the review's
    /// log must be asked again: the turn its last replay caught running has ended.
    pub fn apply(&mut self, tell: &AgentTell) -> bool {
        self.0
            .get_mut(tell.pr())
            .is_some_and(|chat| chat.apply(tell))
    }

    pub fn replay(&mut self, pr: &PrRef, entries: &[AgentLogEntry]) {
        if let Some(chat) = self.0.get_mut(pr) {
            chat.replay(entries);
        }
    }

    /// A permission request of `pr` ended; a review without a chat ignores it (its log has it).
    pub fn permission(
        &mut self,
        pr: &PrRef,
        tool: &str,
        summary: &str,
        outcome: PermissionOutcome,
    ) {
        if let Some(chat) = self.0.get_mut(pr) {
            chat.permission(tool, summary, outcome);
        }
    }

    /// The rules the daemon says `pr` has granted.
    pub fn set_rules(&mut self, pr: &PrRef, rules: Vec<String>) {
        if let Some(chat) = self.0.get_mut(pr) {
            chat.rules = rules;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_protocol::{AgentErrorKind, AgentLogEntry, SessionStateKind, Suggestion};

    fn pr() -> PrRef {
        "rzorzal/clusia#123".parse().unwrap()
    }

    fn suggestion(id: &str, line: u32) -> Suggestion {
        Suggestion {
            id: id.into(),
            file: "src/auth/refresh.rs".into(),
            line: Some(line),
            start_line: None,
            end_line: None,
            body: "Re-check `expires_at` after taking the lock.".into(),
        }
    }

    fn chunk(turn: u64, text: &str) -> AgentTell {
        AgentTell::Chunk {
            pr: pr(),
            turn,
            text: text.into(),
        }
    }

    fn chats() -> Chats {
        let mut chats = Chats::default();
        chats.entry(&pr());
        chats
    }

    #[test]
    fn chunks_of_one_turn_are_one_answer_and_a_tool_line_splits_it() {
        let mut chats = chats();
        for tell in [
            chunk(1, "The lock "),
            chunk(1, "is needed."),
            AgentTell::ToolUse {
                pr: pr(),
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
            chunk(1, "Re-check after it."),
            chunk(2, "A new turn."),
        ] {
            chats.apply(&tell);
        }
        assert_eq!(
            chats.0[&pr()].lines,
            [
                ChatLine::Text("The lock is needed.".into()),
                ChatLine::Tool("Read src/auth/store.rs".into()),
                ChatLine::Text("Re-check after it.".into()),
                ChatLine::Text("A new turn.".into()),
            ]
        );
    }

    #[test]
    fn events_for_a_review_without_a_chat_are_ignored() {
        let mut chats = Chats::default();
        chats.apply(&chunk(1, "nobody is listening"));
        assert!(chats.0.is_empty());
        assert_eq!(chats.state(&pr()), SessionStateKind::None);
    }

    #[test]
    fn every_kind_of_event_makes_its_line() {
        let mut chats = chats();
        let tells = [
            AgentTell::Denied {
                pr: pr(),
                turn: 1,
                tool: "Bash".into(),
                detail: "cargo test".into(),
            },
            AgentTell::Suggestion {
                pr: pr(),
                turn: 1,
                suggestion: suggestion("sug-aaaaaaaaaaaa", 44),
            },
            AgentTell::Error {
                pr: pr(),
                turn: 1,
                kind: AgentErrorKind::NotSignedIn,
                message: String::new(),
            },
            AgentTell::Error {
                pr: pr(),
                turn: 2,
                kind: AgentErrorKind::UsageLimit,
                message: "You hit your limit".into(),
            },
            AgentTell::Refused {
                pr: pr(),
                message: "no review for rzorzal/clusia#123; open it first".into(),
            },
        ];
        for t in &tells {
            chats.apply(t);
        }
        assert_eq!(
            chats.0[&pr()].lines,
            [
                ChatLine::Denied {
                    tool: "Bash".into(),
                    detail: "cargo test".into()
                },
                ChatLine::Suggestion {
                    suggestion: suggestion("sug-aaaaaaaaaaaa", 44),
                    state: SuggestionState::Waiting
                },
                ChatLine::Error("Run `claude` once in a terminal".into()),
                ChatLine::Error("You hit your limit".into()),
                ChatLine::Error("no review for rzorzal/clusia#123; open it first".into()),
            ]
        );
        assert_eq!(chats.0[&pr()].waiting(), 1);
    }

    #[test]
    fn state_events_and_handled_suggestions_update_the_model() {
        let mut chats = chats();
        chats.apply(&AgentTell::State {
            pr: pr(),
            state: SessionStateKind::Running,
        });
        assert!(chats.0[&pr()].busy());
        assert_eq!(chats.state(&pr()), SessionStateKind::Running);
        chats.apply(&AgentTell::Suggestion {
            pr: pr(),
            turn: 1,
            suggestion: suggestion("sug-aaaaaaaaaaaa", 44),
        });
        chats.apply(&AgentTell::Suggestion {
            pr: pr(),
            turn: 1,
            suggestion: suggestion("sug-bbbbbbbbbbbb", 50),
        });
        chats.apply(&AgentTell::Handled {
            pr: pr(),
            id: "sug-aaaaaaaaaaaa".into(),
            accepted: true,
        });
        chats.apply(&AgentTell::Handled {
            pr: pr(),
            id: "sug-bbbbbbbbbbbb".into(),
            accepted: false,
        });
        let states: Vec<SuggestionState> = chats.0[&pr()]
            .lines
            .iter()
            .filter_map(|l| match l {
                ChatLine::Suggestion { state, .. } => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            [SuggestionState::Accepted, SuggestionState::Dismissed]
        );
        assert_eq!(chats.0[&pr()].waiting(), 0);
    }

    #[test]
    fn the_first_words_of_the_agent_switch_to_its_tab_unless_the_user_chose() {
        let mut chats = chats();
        assert_eq!(
            chats.0[&pr()].tab,
            PanelTab::Draft,
            "no harness is known yet"
        );
        chats.apply(&AgentTell::State {
            pr: pr(),
            state: SessionStateKind::Running,
        });
        assert_eq!(
            chats.0[&pr()].tab,
            PanelTab::Draft,
            "a state change alone says nothing"
        );
        chats.apply(&chunk(1, "Summary."));
        assert_eq!(chats.0[&pr()].tab, PanelTab::Agent);

        let mut chosen = Chats::default();
        chosen.entry(&pr()).show(PanelTab::Draft);
        chosen.apply(&chunk(1, "Summary."));
        assert_eq!(
            chosen.0[&pr()].tab,
            PanelTab::Draft,
            "the user chose the draft"
        );
    }

    #[test]
    fn the_panel_is_shown_until_it_is_hidden() {
        let mut chat = ChatModel::default();
        assert!(chat.open);
        chat.toggle();
        assert!(!chat.open);
        chat.toggle();
        assert!(chat.open);
    }

    #[test]
    fn replay_rebuilds_the_chat() {
        let entries = vec![
            AgentLogEntry::User {
                at: 1,
                turn: 1,
                text: "Is the lock needed?".into(),
            },
            AgentLogEntry::ToolUse {
                at: 2,
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
            AgentLogEntry::Text {
                at: 3,
                turn: 1,
                text: "The lock ".into(),
            },
            AgentLogEntry::Text {
                at: 4,
                turn: 1,
                text: "is needed.".into(),
            },
            AgentLogEntry::Suggestion {
                at: 5,
                turn: 1,
                suggestion: suggestion("sug-aaaaaaaaaaaa", 44),
            },
            AgentLogEntry::Done {
                at: 6,
                turn: 1,
                duration_ms: 1800,
            },
        ];
        let mut replayed = chats();
        replayed.apply(&chunk(9, "events before the log"));
        replayed.replay(&pr(), &entries);
        let chat = &replayed.0[&pr()];
        assert_eq!(
            chat.lines,
            [
                ChatLine::Me("Is the lock needed?".into()),
                ChatLine::Tool("Read src/auth/store.rs".into()),
                ChatLine::Text("The lock is needed.".into()),
                ChatLine::Suggestion {
                    suggestion: suggestion("sug-aaaaaaaaaaaa", 44),
                    state: SuggestionState::Waiting
                },
            ]
        );
        assert!(chat.resumed);

        let mut running = chats();
        running.replay(&pr(), &entries[..3]);
        running.apply(&chunk(1, "is needed."));
        assert_eq!(
            running.0[&pr()].lines.last(),
            Some(&ChatLine::Text("The lock is needed.".into())),
            "the running answer goes on in the same bubble"
        );

        let mut empty = chats();
        empty.replay(&pr(), &[]);
        assert!(!empty.0[&pr()].resumed);
    }

    #[test]
    fn a_replay_keeps_the_lines_only_this_window_has() {
        let s = suggestion("sug-aaaaaaaaaaaa", 44);
        let question = AgentLogEntry::User {
            at: 1,
            turn: 1,
            text: "Is the lock needed?".into(),
        };
        let answer = AgentLogEntry::Text {
            at: 2,
            turn: 1,
            text: "Yes.".into(),
        };
        let done = AgentLogEntry::Done {
            at: 4,
            turn: 1,
            duration_ms: 900,
        };
        let mut chats = chats();
        chats.replay(
            &pr(),
            &[
                question.clone(),
                answer.clone(),
                AgentLogEntry::Suggestion {
                    at: 3,
                    turn: 1,
                    suggestion: s.clone(),
                },
                done.clone(),
            ],
        );
        chats.apply(&AgentTell::Handled {
            pr: pr(),
            id: s.id.clone(),
            accepted: true,
        });
        chats.entry(&pr()).push_me("And now?".into());
        chats.apply(&AgentTell::Refused {
            pr: pr(),
            message: "The agent is busy".into(),
        });
        chats.entry(&pr()).push_error("Not connected".into());
        // The daemon's log leaves out a handled suggestion and never had the refused send.
        chats.replay(&pr(), &[question, answer, done]);
        assert_eq!(
            chats.0[&pr()].lines,
            [
                ChatLine::Me("Is the lock needed?".into()),
                ChatLine::Text("Yes.".into()),
                ChatLine::Suggestion {
                    suggestion: s,
                    state: SuggestionState::Accepted
                },
                ChatLine::Me("And now?".into()),
                ChatLine::Error("The agent is busy".into()),
                ChatLine::Error("Not connected".into()),
            ]
        );
    }

    #[test]
    fn a_replay_keeps_the_state_the_daemon_told() {
        let mut chats = chats();
        chats.apply(&AgentTell::State {
            pr: pr(),
            state: SessionStateKind::Queued,
        });
        chats.replay(
            &pr(),
            &[AgentLogEntry::User {
                at: 1,
                turn: 1,
                text: "Is the lock needed?".into(),
            }],
        );
        assert_eq!(chats.state(&pr()), SessionStateKind::Queued);
        assert!(chats.0[&pr()].busy(), "the Stop button stays");
    }

    #[test]
    fn a_turn_whose_end_never_reaches_the_log_is_asked_again_once() {
        // A turn cut by the end of its review writes no end: its log stays unfinished.
        let open = [AgentLogEntry::User {
            at: 1,
            turn: 1,
            text: "Is the lock needed?".into(),
        }];
        let none = AgentTell::State {
            pr: pr(),
            state: SessionStateKind::None,
        };
        let mut chats = chats();
        chats.replay(&pr(), &open);
        assert!(chats.apply(&none));
        chats.replay(&pr(), &open);
        assert!(!chats.apply(&none), "no loop of log reads");
    }

    #[test]
    fn the_shown_text_hides_the_suggestion_blocks_only() {
        let whole = "Use a lock.\n\n```clusia-suggestion\n{\"file\":\"a.rs\",\"line\":3,\"body\":\"x\"}\n```\n";
        assert_eq!(display_text(whole), "Use a lock.");
        let streaming = "Use a lock.\n\n```clusia-suggestion\n{\"file\":\"a.rs\",\"li";
        assert_eq!(display_text(streaming), "Use a lock.");
        let invalid = "Look:\n\n```clusia-suggestion\nnot json\n```\n";
        assert!(
            display_text(invalid).contains("not json"),
            "an invalid block stays as text"
        );
        assert_eq!(display_text("Plain **text**."), "Plain **text**.");
    }

    #[test]
    fn places_read_like_the_draft_cards() {
        assert_eq!(place_of(&suggestion("s", 44)), "refresh.rs:44");
        let mut range = suggestion("s", 0);
        range.line = None;
        range.start_line = Some(40);
        range.end_line = Some(44);
        assert_eq!(place_of(&range), "refresh.rs:40–44");
        range.start_line = None;
        range.end_line = None;
        assert_eq!(place_of(&range), "refresh.rs");
    }

    #[test]
    fn the_panel_stays_within_its_bounds() {
        assert_eq!(resized(DEFAULT_WIDTH, -30.0), DEFAULT_WIDTH + 30.0);
        assert_eq!(resized(DEFAULT_WIDTH, 10_000.0), MIN_WIDTH);
        assert_eq!(resized(DEFAULT_WIDTH, -10_000.0), MAX_WIDTH);
    }

    #[test]
    fn the_end_of_the_turn_a_replay_caught_running_asks_the_log_again_once() {
        let mut caught = chats();
        caught.replay(
            &pr(),
            &[
                AgentLogEntry::User {
                    at: 1,
                    turn: 1,
                    text: "Is the lock needed?".into(),
                },
                AgentLogEntry::ToolUse {
                    at: 2,
                    turn: 1,
                    summary: "Read src/auth/store.rs".into(),
                },
            ],
        );
        assert!(!caught.apply(&chunk(1, "only the tail of the answer")));
        let done = AgentTell::Done {
            pr: pr(),
            turn: 1,
            duration_ms: 900,
        };
        assert!(caught.apply(&done), "the log now has the whole answer");
        assert!(!caught.apply(&done), "asked once");

        let mut errored = chats();
        errored.replay(
            &pr(),
            &[AgentLogEntry::User {
                at: 1,
                turn: 2,
                text: "And the refresh?".into(),
            }],
        );
        assert!(errored.apply(&AgentTell::Error {
            pr: pr(),
            turn: 2,
            kind: AgentErrorKind::Crashed,
            message: String::new(),
        }));
    }

    #[test]
    fn a_finished_replay_or_another_turn_ending_asks_nothing() {
        let mut chats = chats();
        chats.replay(
            &pr(),
            &[
                AgentLogEntry::User {
                    at: 1,
                    turn: 1,
                    text: "Is the lock needed?".into(),
                },
                AgentLogEntry::Done {
                    at: 2,
                    turn: 1,
                    duration_ms: 900,
                },
            ],
        );
        assert!(!chats.apply(&AgentTell::Done {
            pr: pr(),
            turn: 2,
            duration_ms: 900,
        }));
    }

    #[test]
    fn a_ready_state_ends_a_replay_caught_running_whose_end_was_missed() {
        // The log was read before the turn's end was written, and its `Done` came while the
        // log was in flight: only the state says the turn is over.
        let mut caught = chats();
        caught.replay(
            &pr(),
            &[
                AgentLogEntry::User {
                    at: 1,
                    turn: 1,
                    text: "Is the lock needed?".into(),
                },
                AgentLogEntry::Text {
                    at: 2,
                    turn: 1,
                    text: "Yes, because".into(),
                },
            ],
        );
        let ready = AgentTell::State {
            pr: pr(),
            state: SessionStateKind::Ready,
        };
        assert!(
            caught.apply(&ready),
            "the log is read again for the whole answer"
        );
        assert_eq!(caught.state(&pr()), SessionStateKind::Ready);
        assert!(!caught.apply(&ready), "asked once");
        assert!(
            !caught.apply(&AgentTell::Done {
                pr: pr(),
                turn: 1,
                duration_ms: 900,
            }),
            "a late end of that turn asks nothing more"
        );

        let mut queued = chats();
        queued.replay(
            &pr(),
            &[AgentLogEntry::User {
                at: 1,
                turn: 2,
                text: "And the refresh?".into(),
            }],
        );
        assert!(
            !queued.apply(&AgentTell::State {
                pr: pr(),
                state: SessionStateKind::Queued,
            }),
            "a turn still waiting keeps its marker"
        );
        assert!(queued.apply(&AgentTell::State {
            pr: pr(),
            state: SessionStateKind::None,
        }));
    }

    #[test]
    fn the_log_replays_permission_lines_in_order() {
        use clusia_protocol::PermissionOutcome;
        let mut chats = chats();
        chats.replay(
            &pr(),
            &[
                AgentLogEntry::User {
                    at: 1,
                    turn: 1,
                    text: "Run the tests".into(),
                },
                AgentLogEntry::Permission {
                    at: 2,
                    turn: 1,
                    tool: "Bash".into(),
                    summary: "cargo test".into(),
                    outcome: PermissionOutcome::AllowedForReview,
                },
                AgentLogEntry::Done {
                    at: 3,
                    turn: 1,
                    duration_ms: 10,
                },
            ],
        );
        assert_eq!(
            chats.0[&pr()].lines,
            [
                ChatLine::Me("Run the tests".into()),
                ChatLine::Permission {
                    tool: "Bash".into(),
                    summary: "cargo test".into(),
                    outcome: PermissionOutcome::AllowedForReview,
                },
            ]
        );
    }

    #[test]
    fn a_turn_that_ends_on_a_permission_line_is_still_running_for_the_replay() {
        use clusia_protocol::PermissionOutcome;
        let mut chats = chats();
        chats.replay(
            &pr(),
            &[AgentLogEntry::Permission {
                at: 2,
                turn: 4,
                tool: "Bash".into(),
                summary: "cargo test".into(),
                outcome: PermissionOutcome::Allowed,
            }],
        );
        // Its `Done` arrives later: the log is read again once.
        assert!(chats.apply(&AgentTell::Done {
            pr: pr(),
            turn: 4,
            duration_ms: 5
        }));
    }

    #[test]
    fn live_permission_lines_and_rules_belong_to_an_open_chat() {
        use clusia_protocol::PermissionOutcome;
        let mut chats = Chats::default();
        chats.permission(&pr(), "Bash", "cargo test", PermissionOutcome::Denied);
        chats.set_rules(&pr(), vec!["Bash(cargo test:*)".into()]);
        assert!(
            chats.0.is_empty(),
            "no chat, nothing kept: its log has them"
        );
        chats.entry(&pr());
        chats.permission(&pr(), "Bash", "cargo test", PermissionOutcome::Denied);
        chats.set_rules(&pr(), vec!["Bash(cargo test:*)".into()]);
        let chat = &chats.0[&pr()];
        assert_eq!(
            chat.lines,
            [ChatLine::Permission {
                tool: "Bash".into(),
                summary: "cargo test".into(),
                outcome: PermissionOutcome::Denied,
            }]
        );
        assert_eq!(chat.rules, ["Bash(cargo test:*)"]);
    }
}

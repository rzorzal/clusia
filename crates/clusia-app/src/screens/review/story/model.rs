//! What the window knows of each open review's Story: the versions the daemon lists, the ones it
//! sent whole, the static order, the read marks and what the daemon is doing. The bridge fills it
//! from `StoryTell`s; the tab reads it, and only the marks are written here (optimistically).

use std::collections::{BTreeSet, HashMap};

use bevy::prelude::*;
use clusia_calls::StaticStep;
use clusia_core::PrRef;
use clusia_core::story::StoryVersion;
use clusia_protocol::{StoryState, StoryVersionInfo};

use crate::bridge::StoryTell;

/// One review's Story.
#[derive(Debug, Clone, PartialEq)]
pub struct StoryModel {
    /// `GetStory` was asked for.
    pub asked: bool,
    /// The daemon's answer arrived.
    pub received: bool,
    /// Every version the daemon lists, oldest first, without specs.
    pub versions: Vec<StoryVersionInfo>,
    /// The versions whole: the latest, and the older ones the reviewer stepped to.
    pub loaded: HashMap<u32, StoryVersion>,
    /// Older versions asked for and not answered yet.
    pub requested: BTreeSet<u32>,
    /// Older versions the daemon refused to send, until the tab steps to them again.
    pub failed: BTreeSet<u32>,
    pub state: StoryState,
    /// The static order the index gives; it shows while there is no version.
    pub static_steps: Vec<StaticStep>,
    /// Step ids read in the latest version, or in the static steps when there is none.
    pub read: BTreeSet<String>,
    /// The review's head commit.
    pub head: String,
    /// Commits from the latest version's commit to the head; `0` with no version.
    pub new_commits: u32,
    /// A `WriteStory` went out since the last answer.
    pub write_sent: bool,
}

impl Default for StoryModel {
    fn default() -> Self {
        Self {
            asked: false,
            received: false,
            versions: Vec::new(),
            loaded: HashMap::new(),
            requested: BTreeSet::new(),
            failed: BTreeSet::new(),
            state: StoryState::Idle,
            static_steps: Vec::new(),
            read: BTreeSet::new(),
            head: String::new(),
            new_commits: 0,
            write_sent: false,
        }
    }
}

impl StoryModel {
    pub fn latest_number(&self) -> Option<u32> {
        self.versions.iter().map(|v| v.number).max()
    }

    pub fn latest(&self) -> Option<&StoryVersion> {
        self.loaded.get(&self.latest_number()?)
    }

    /// The agent is writing, or waits for a place to write.
    pub fn busy(&self) -> bool {
        matches!(self.state, StoryState::Writing | StoryState::Waiting)
    }

    /// The pull request moved on since the latest version was written.
    pub fn outdated(&self) -> bool {
        !self.head.is_empty() && self.latest().is_some_and(|v| v.commit != self.head)
    }

    /// Marks or unmarks a step before the daemon answers; `ReadRefused` undoes it.
    pub fn mark(&mut self, step: &str, read: bool) {
        if read {
            self.read.insert(step.to_string());
        } else {
            self.read.remove(step);
        }
    }

    pub fn apply(&mut self, tell: &StoryTell) {
        match tell {
            StoryTell::Loaded {
                versions,
                latest,
                state,
                static_steps,
                read,
                head,
                new_commits,
                ..
            } => {
                self.versions = versions.clone();
                self.versions.sort_by_key(|v| v.number);
                if let Some(latest) = latest {
                    self.loaded.insert(latest.number, (**latest).clone());
                }
                let listed: Vec<u32> = self.versions.iter().map(|v| v.number).collect();
                self.loaded.retain(|n, _| listed.contains(n));
                self.requested.retain(|n| !self.loaded.contains_key(n));
                self.failed.retain(|n| listed.contains(n));
                self.state = state.clone();
                self.static_steps = static_steps.clone();
                self.read = read.iter().cloned().collect();
                self.head = head.clone();
                self.new_commits = *new_commits;
                self.write_sent = false;
                self.asked = true;
                self.received = true;
            }
            StoryTell::Version { version, .. } => {
                let asked = self.requested.remove(&version.number);
                // A reply that outlived a shorter listing would bring a dropped version back.
                if asked || self.versions.iter().any(|v| v.number == version.number) {
                    if self.versions.iter().any(|v| v.number == version.number) {
                        self.loaded.insert(version.number, (**version).clone());
                    }
                    self.failed.remove(&version.number);
                }
            }
            StoryTell::Unreadable { number, .. } => {
                self.requested.remove(number);
                self.failed.insert(*number);
            }
            StoryTell::State { state, .. } => {
                let was_busy = self.busy();
                self.state = state.clone();
                self.write_sent = false;
                // The end of a run left a version (or a failure) the model has not read: the
                // old answer is stale until the new one arrives, so nothing writes meanwhile.
                if was_busy && !self.busy() {
                    self.asked = false;
                    self.received = false;
                }
            }
            StoryTell::ReadRefused { step, read, .. } => self.mark(step, !*read),
        }
    }
}

/// Every review's Story.
#[derive(Resource, Debug, Default)]
pub struct Stories(pub HashMap<PrRef, StoryModel>);

impl Stories {
    pub fn apply(&mut self, tell: &StoryTell) {
        self.0.entry(tell.pr().clone()).or_default().apply(tell);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{StoryTell, Tell};
    use crate::fixture;
    use crate::testing::NOW;

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn loaded() -> StoryModel {
        let mut m = StoryModel::default();
        let Tell::Story(tell) = fixture::demo_story_tell(&pr(), NOW) else {
            panic!("a story tell");
        };
        m.apply(&tell);
        m
    }

    #[test]
    fn loaded_holds_the_latest_version_whole_and_lists_both() {
        let m = loaded();
        assert!(m.asked && m.received);
        assert_eq!(
            m.versions.iter().map(|v| v.number).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(m.latest_number(), Some(2));
        assert_eq!(m.loaded.keys().copied().collect::<Vec<_>>(), [2]);
        assert_eq!(
            m.latest().map(|v| v.spec.as_ref().map(|s| s.steps.len())),
            Some(Some(6))
        );
        assert_eq!(m.static_steps.len(), 6);
        assert_eq!(
            m.read.iter().cloned().collect::<Vec<_>>(),
            ["refresh-early"]
        );
        assert_eq!(m.state, StoryState::Idle);
        assert_eq!(m.new_commits, 0);
        assert!(!m.busy() && !m.outdated());
    }

    #[test]
    fn the_count_of_new_commits_follows_each_answer() {
        let mut m = loaded();
        let Tell::Story(StoryTell::Loaded {
            pr,
            versions,
            latest,
            state,
            static_steps,
            read,
            head,
            ..
        }) = fixture::demo_story_tell(&pr(), NOW)
        else {
            panic!("a loaded story");
        };
        m.apply(&StoryTell::Loaded {
            pr,
            versions,
            latest,
            state,
            static_steps,
            read,
            head,
            new_commits: 2,
        });
        assert_eq!(m.new_commits, 2);
        m.apply(&StoryTell::Loaded {
            pr: fixture::demo_pr(),
            versions: Vec::new(),
            latest: None,
            state: StoryState::Idle,
            static_steps: Vec::new(),
            read: Vec::new(),
            head: String::new(),
            new_commits: 0,
        });
        assert_eq!(m.new_commits, 0, "no version, nothing is behind");
    }

    #[test]
    fn an_older_version_arrives_on_request_and_a_refusal_is_remembered() {
        let mut m = loaded();
        m.requested.insert(1);
        let first = fixture::demo_story_version(NOW, 1).expect("version 1");
        m.apply(&StoryTell::Version {
            pr: pr(),
            version: Box::new(first),
        });
        assert!(m.requested.is_empty() && m.loaded.contains_key(&1));
        assert_eq!(m.versions.len(), 2, "its listing is not added twice");
        m.requested.insert(2);
        m.apply(&StoryTell::Unreadable {
            pr: pr(),
            number: 2,
        });
        assert!(m.requested.is_empty() && m.failed.contains(&2));
        let mut other = fixture::demo_story_version(NOW, 1).expect("version 1");
        other.number = 9;
        m.apply(&StoryTell::Version {
            pr: pr(),
            version: Box::new(other),
        });
        assert!(!m.loaded.contains_key(&9), "an unlisted version is ignored");
    }

    #[test]
    fn the_end_of_a_run_reads_the_daemon_again() {
        let mut m = loaded();
        m.write_sent = true;
        m.apply(&StoryTell::State {
            pr: pr(),
            state: StoryState::Writing,
        });
        assert!(
            m.busy() && m.asked && !m.write_sent,
            "a new state allows a new trigger"
        );
        m.apply(&StoryTell::State {
            pr: pr(),
            state: StoryState::Waiting,
        });
        assert!(m.busy());
        m.apply(&StoryTell::State {
            pr: pr(),
            state: StoryState::Idle,
        });
        assert!(
            !m.busy() && !m.asked && !m.received,
            "the model asks for the final versions again and waits for them"
        );
        m.asked = true;
        m.apply(&StoryTell::State {
            pr: pr(),
            state: StoryState::Idle,
        });
        assert!(m.asked, "an Idle after Idle reads nothing");
    }

    #[test]
    fn a_version_for_an_older_commit_than_the_head_is_outdated() {
        let mut m = loaded();
        m.head = "feedfacefeedfacefeedfacefeedfacefeedface".into();
        assert!(m.outdated());
        m.head.clear();
        assert!(!m.outdated(), "no head known, nothing to compare");
        assert!(!StoryModel::default().outdated());
    }

    #[test]
    fn marks_are_optimistic_and_a_refusal_undoes_them() {
        let mut m = loaded();
        m.mark("one-at-a-time", true);
        assert!(m.read.contains("one-at-a-time"));
        m.apply(&StoryTell::ReadRefused {
            pr: pr(),
            step: "one-at-a-time".into(),
            read: true,
        });
        assert!(!m.read.contains("one-at-a-time"));
        m.mark("refresh-early", false);
        assert!(!m.read.contains("refresh-early"));
        m.apply(&StoryTell::ReadRefused {
            pr: pr(),
            step: "refresh-early".into(),
            read: false,
        });
        assert!(m.read.contains("refresh-early"));
    }

    #[test]
    fn the_resource_keeps_one_model_per_review() {
        let mut stories = Stories::default();
        stories.apply(&StoryTell::State {
            pr: pr(),
            state: StoryState::Writing,
        });
        let other = PrRef::new("acme", "widgets", 7).expect("ref");
        stories.apply(&StoryTell::State {
            pr: other.clone(),
            state: StoryState::Idle,
        });
        assert!(stories.0[&pr()].busy() && !stories.0[&other].busy());
    }
}

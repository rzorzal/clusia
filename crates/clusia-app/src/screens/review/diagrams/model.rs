//! What the window knows of each open review's diagrams: the versions the daemon lists, the
//! drawings it has sent whole, and what the daemon is doing now. The bridge fills it from
//! `DiagramsTell`s; the tab reads it and never writes to it.

use std::collections::{BTreeSet, HashMap};

use bevy::prelude::*;
use clusia_core::PrRef;
use clusia_core::diagram::{
    DiagramEntry, DiagramVersion, DiagramVersionInfo, EntryInfo, EntryStateName,
};
use clusia_protocol::{DiagramCode, DiagramsState};

use crate::bridge::DiagramsTell;
use crate::review_state::TabUi;

/// How many boxes' code the model keeps.
const MAX_CODES: usize = 64;

/// One review's diagrams.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagramsModel {
    /// `GetDiagrams` was asked for.
    pub asked: bool,
    /// The daemon's answer arrived.
    pub received: bool,
    /// Every version the daemon lists, oldest first, without specs.
    pub versions: Vec<DiagramVersionInfo>,
    /// The versions whole: the latest one, and the older ones the reviewer stepped to.
    pub loaded: HashMap<u32, DiagramVersion>,
    /// Older versions asked for and not answered yet.
    pub requested: BTreeSet<u32>,
    pub state: DiagramsState,
    /// The review's head commit.
    pub head: String,
    /// The pull request moved on since the latest version was drawn.
    pub stale: bool,
    pub stale_reason: Option<String>,
    /// The code `ReadDiagramCode` answered, by version number, diagram key and box id.
    pub code: HashMap<(u32, String, String), DiagramCode>,
}

impl Default for DiagramsModel {
    fn default() -> Self {
        Self {
            asked: false,
            received: false,
            versions: Vec::new(),
            loaded: HashMap::new(),
            requested: BTreeSet::new(),
            state: DiagramsState::Idle,
            head: String::new(),
            stale: false,
            stale_reason: None,
            code: HashMap::new(),
        }
    }
}

impl DiagramsModel {
    pub fn latest_number(&self) -> Option<u32> {
        self.versions.iter().map(|v| v.number).max()
    }

    /// A plan or a draw turn is running.
    pub fn busy(&self) -> bool {
        matches!(
            self.state,
            DiagramsState::Planning | DiagramsState::Drawing { .. }
        )
    }

    /// Forgets the code of every box of `new` that is not the same as the box of the version of
    /// that number held now: a redraw keeps the number, but its lines and marks are another's.
    fn forget_changed_code(&mut self, new: &DiagramVersion) {
        let old = self.loaded.get(&new.number);
        let same_commit = old.is_some_and(|o| o.commit == new.commit);
        let stale: Vec<String> = new
            .entries
            .iter()
            .filter(|e| {
                !same_commit
                    || old.and_then(|o| o.entry(&e.plan.key)).map(|o| &o.state) != Some(&e.state)
            })
            .map(|e| e.plan.key.clone())
            .collect();
        self.code
            .retain(|(n, k, _), _| *n != new.number || !stale.contains(k));
    }

    fn upsert(&mut self, info: DiagramVersionInfo) {
        match self.versions.iter_mut().find(|v| v.number == info.number) {
            Some(slot) => *slot = info,
            None => self.versions.push(info),
        }
        self.versions.sort_by_key(|v| v.number);
    }

    pub fn apply(&mut self, tell: &DiagramsTell) {
        match tell {
            DiagramsTell::Loaded {
                versions,
                latest,
                state,
                head,
                stale,
                stale_reason,
                ..
            } => {
                self.versions = versions.clone();
                self.versions.sort_by_key(|v| v.number);
                if let Some(latest) = latest {
                    self.forget_changed_code(latest);
                    self.loaded.insert(latest.number, (**latest).clone());
                }
                let listed: Vec<u32> = self.versions.iter().map(|v| v.number).collect();
                self.loaded.retain(|n, _| listed.contains(n));
                self.code.retain(|(n, _, _), _| listed.contains(n));
                self.requested.retain(|n| !self.loaded.contains_key(n));
                self.state = state.clone();
                self.head = head.clone();
                self.stale = *stale;
                self.stale_reason = stale_reason.clone();
                self.asked = true;
                self.received = true;
            }
            DiagramsTell::Version { version, .. } => {
                self.requested.remove(&version.number);
                self.forget_changed_code(version);
                self.upsert(version.info());
                self.loaded.insert(version.number, (**version).clone());
            }
            DiagramsTell::State { state, .. } => {
                let was_busy = self.busy();
                self.state = state.clone();
                // A run that ended leaves the daemon's last word to read again.
                if was_busy && !self.busy() {
                    self.asked = false;
                }
            }
            DiagramsTell::Planned { version, .. } => {
                self.forget_changed_code(version);
                self.upsert(version.info());
                self.loaded.insert(version.number, (**version).clone());
                if !self.head.is_empty() && version.commit == self.head {
                    self.stale = false;
                    self.stale_reason = None;
                }
            }
            DiagramsTell::Entry {
                number, key, state, ..
            } => {
                let Some(version) = self.loaded.get_mut(number) else {
                    return;
                };
                if let Some(entry) = version.entries.iter_mut().find(|e| e.plan.key == *key) {
                    entry.state = state.clone();
                }
                self.code.retain(|(n, k, _), _| !(n == number && k == key));
                let info = version.info();
                self.upsert(info);
            }
            DiagramsTell::Code {
                number,
                key,
                node,
                code,
                ..
            } => {
                // A reviewer opens a handful of boxes; the answers of a long session do not pile up.
                if self.code.len() >= MAX_CODES {
                    self.code.clear();
                }
                self.code
                    .insert((*number, key.clone(), node.clone()), code.clone());
            }
        }
    }

    /// The listing of the version the tab shows (`version`, else the latest), and whether that
    /// is the latest one. An unknown number falls back to the latest.
    pub fn shown(&self, version: Option<u32>) -> Option<(&DiagramVersionInfo, bool)> {
        let latest = self.latest_number()?;
        let number = version
            .filter(|n| self.versions.iter().any(|v| v.number == *n))
            .unwrap_or(latest);
        let info = self.versions.iter().find(|v| v.number == number)?;
        Some((info, number == latest))
    }

    /// The version before `number` that drew `key` successfully, which *Show changes since*
    /// compares with. `None` for version 1 and for a diagram that is new in `number`.
    pub fn compare_with(&self, number: u32, key: &str) -> Option<u32> {
        let ready = |v: &DiagramVersionInfo| {
            v.entries
                .iter()
                .any(|e| e.key == key && e.state == EntryStateName::Ready)
        };
        let current = self.versions.iter().find(|v| v.number == number)?;
        if !ready(current) {
            return None;
        }
        self.versions
            .iter()
            .filter(|v| v.number < number && ready(v))
            .map(|v| v.number)
            .max()
    }
}

/// The key of the selected entry in `info`: `selected` when that version draws it, else the
/// first entry that is ready, else the first one.
pub fn selected_key(info: &DiagramVersionInfo, selected: Option<&str>) -> Option<String> {
    let entry: Option<&EntryInfo> = selected
        .and_then(|k| info.entries.iter().find(|e| e.key == k))
        .or_else(|| {
            info.entries
                .iter()
                .find(|e| e.state == EntryStateName::Ready)
        })
        .or(info.entries.first());
    entry.map(|e| e.key.clone())
}

/// What the tab shows of a drawing: the version, whole, and the selected entry of it.
#[derive(Debug, Clone, Copy)]
pub struct Picked<'a> {
    pub number: u32,
    pub latest: bool,
    pub version: &'a DiagramVersion,
    pub entry: &'a DiagramEntry,
}

/// The selected entry of the version `ui` points at, once that version is loaded. The canvas
/// draws from this.
pub fn picked<'a>(model: &'a DiagramsModel, ui: &TabUi) -> Option<Picked<'a>> {
    let (info, latest) = model.shown(ui.diagram_version)?;
    let key = selected_key(info, ui.diagram.as_deref())?;
    let version = model.loaded.get(&info.number)?;
    let entry = version.entry(&key)?;
    Some(Picked {
        number: info.number,
        latest,
        version,
        entry,
    })
}

/// Every review's diagrams.
#[derive(Resource, Debug, Default)]
pub struct Diagrams(pub HashMap<PrRef, DiagramsModel>);

impl Diagrams {
    pub fn apply(&mut self, tell: &DiagramsTell) {
        self.0.entry(tell.pr().clone()).or_default().apply(tell);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::NOW;
    use clusia_core::diagram::EntryState;

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn loaded() -> DiagramsModel {
        let mut m = DiagramsModel::default();
        let Tell::Diagrams(tell) = fixture::demo_diagrams_tell(&pr(), NOW) else {
            panic!("a diagrams tell");
        };
        m.apply(&tell);
        m
    }

    use crate::bridge::Tell;

    #[test]
    fn loaded_lists_both_versions_and_holds_the_latest_whole() {
        let m = loaded();
        assert!(m.asked && m.received);
        assert_eq!(
            m.versions.iter().map(|v| v.number).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(m.latest_number(), Some(2));
        assert_eq!(m.loaded.keys().copied().collect::<Vec<_>>(), [2]);
        assert_eq!(m.state, DiagramsState::Idle);
        assert!(!m.stale && !m.busy());
        assert_eq!(m.versions[1].entries.len(), 8);
        assert_eq!(m.versions[0].entries.len(), 3);
    }

    #[test]
    fn an_older_version_arrives_whole_on_request() {
        let mut m = loaded();
        m.requested.insert(1);
        let first = fixture::demo_diagrams(NOW, &pr()).remove(0);
        m.apply(&DiagramsTell::Version {
            pr: pr(),
            version: Box::new(first),
        });
        assert!(m.requested.is_empty());
        assert!(m.loaded.contains_key(&1));
        assert_eq!(
            m.versions.len(),
            2,
            "its listing is replaced, not added twice"
        );
    }

    #[test]
    fn a_plan_adds_a_version_with_waiting_entries_and_each_entry_updates_it() {
        let mut m = loaded();
        m.head = "feedfacefeedfacefeedfacefeedfacefeedface".into();
        m.stale = true;
        let mut v3 = fixture::demo_diagrams(NOW, &pr()).remove(1);
        v3.number = 3;
        v3.commit = m.head.clone();
        for e in &mut v3.entries {
            e.state = EntryState::Waiting;
        }
        m.apply(&DiagramsTell::Planned {
            pr: pr(),
            version: Box::new(v3),
        });
        assert_eq!(m.latest_number(), Some(3));
        assert!(!m.stale, "the new version is for the head");
        assert!(
            m.versions[2]
                .entries
                .iter()
                .all(|e| e.state == EntryStateName::Waiting)
        );
        m.apply(&DiagramsTell::Entry {
            pr: pr(),
            number: 3,
            key: "flow".into(),
            state: EntryState::Failed {
                reason: "timed out".into(),
            },
        });
        let flow = &m.versions[2].entries[0];
        assert_eq!(flow.state, EntryStateName::Failed);
        assert_eq!(flow.reason.as_deref(), Some("timed out"));
        m.apply(&DiagramsTell::Entry {
            pr: pr(),
            number: 9,
            key: "flow".into(),
            state: EntryState::Drawing,
        });
        assert_eq!(
            m.versions.len(),
            3,
            "an entry of an unknown version is ignored"
        );
    }

    #[test]
    fn the_end_of_a_run_reads_the_daemon_again() {
        let mut m = loaded();
        m.apply(&DiagramsTell::State {
            pr: pr(),
            state: DiagramsState::Planning,
        });
        assert!(m.busy());
        assert!(m.asked, "starting a run reads nothing");
        m.apply(&DiagramsTell::State {
            pr: pr(),
            state: DiagramsState::Drawing {
                done: 1,
                total: 5,
                key: "flow".into(),
            },
        });
        assert!(m.asked);
        m.apply(&DiagramsTell::State {
            pr: pr(),
            state: DiagramsState::Idle,
        });
        assert!(!m.asked, "the model asks for the final versions again");
    }

    #[test]
    fn a_list_the_daemon_shrinks_drops_the_versions_it_no_longer_has() {
        let mut m = loaded();
        m.loaded
            .insert(1, fixture::demo_diagrams(NOW, &pr()).remove(0));
        let versions = fixture::demo_diagrams(NOW, &pr());
        m.apply(&DiagramsTell::Loaded {
            pr: pr(),
            versions: vec![versions[1].info()],
            latest: Some(Box::new(versions[1].clone())),
            state: DiagramsState::Idle,
            head: versions[1].commit.clone(),
            stale: false,
            stale_reason: None,
        });
        assert_eq!(m.versions.len(), 1);
        assert!(!m.loaded.contains_key(&1));
    }

    fn answer(lines: u32) -> DiagramCode {
        DiagramCode {
            path: "src/auth/refresh.rs".into(),
            commit: "9be1d07c3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d".into(),
            start_line: 38,
            lines: (0..lines)
                .map(|i| clusia_protocol::CodeLine {
                    number: 38 + i,
                    text: format!("line {i}"),
                    changed: i == 1,
                })
                .collect(),
            unavailable: None,
        }
    }

    #[test]
    fn code_is_kept_for_the_box_it_was_asked_about() {
        let mut m = loaded();
        m.apply(&DiagramsTell::Code {
            pr: pr(),
            number: 2,
            key: "flow".into(),
            node: "refresh".into(),
            code: answer(3),
        });
        let key = (2, "flow".to_string(), "refresh".to_string());
        assert_eq!(m.code[&key], answer(3));
        m.apply(&DiagramsTell::Code {
            pr: pr(),
            number: 2,
            key: "flow".into(),
            node: "refresh".into(),
            code: answer(5),
        });
        assert_eq!(m.code.len(), 1, "an answer replaces the one before it");
        assert_eq!(m.code[&key].lines.len(), 5);
    }

    fn ask(m: &mut DiagramsModel, number: u32, key: &str, node: &str) {
        m.apply(&DiagramsTell::Code {
            pr: pr(),
            number,
            key: key.into(),
            node: node.into(),
            code: answer(2),
        });
    }

    #[test]
    fn a_redraw_forgets_the_code_of_its_diagram_and_only_of_its_diagram() {
        let mut m = loaded();
        ask(&mut m, 2, "flow", "refresh");
        ask(&mut m, 2, "sequence", "store");
        let EntryState::Ready { spec, .. } = m.loaded[&2].entry("flow").unwrap().state.clone()
        else {
            panic!("ready");
        };
        m.apply(&DiagramsTell::Entry {
            pr: pr(),
            number: 2,
            key: "flow".into(),
            state: EntryState::Ready {
                spec,
                notes: Vec::new(),
            },
        });
        assert!(!m.code.contains_key(&(2, "flow".into(), "refresh".into())));
        assert!(m.code.contains_key(&(2, "sequence".into(), "store".into())));
    }

    #[test]
    fn a_new_read_of_a_changed_version_forgets_its_code() {
        let mut m = loaded();
        ask(&mut m, 2, "flow", "refresh");
        ask(&mut m, 2, "er", "token");
        let versions = fixture::demo_diagrams(NOW, &pr());
        let again = |v: &DiagramVersion| DiagramsTell::Loaded {
            pr: pr(),
            versions: vec![versions[0].info(), v.info()],
            latest: Some(Box::new(v.clone())),
            state: DiagramsState::Idle,
            head: v.commit.clone(),
            stale: false,
            stale_reason: None,
        };
        m.apply(&again(&versions[1]));
        assert_eq!(m.code.len(), 2, "the same drawing keeps its code");
        let mut redrawn = versions[1].clone();
        redrawn.entries[0].state = EntryState::Failed {
            reason: "timed out".into(),
        };
        m.apply(&again(&redrawn));
        assert!(!m.code.contains_key(&(2, "flow".into(), "refresh".into())));
        assert!(m.code.contains_key(&(2, "er".into(), "token".into())));
        let mut moved = redrawn.clone();
        moved.commit = "feedfacefeedfacefeedfacefeedfacefeedface".into();
        m.apply(&again(&moved));
        assert!(
            m.code.is_empty(),
            "another commit: nothing of version 2 is the same"
        );
    }

    #[test]
    fn a_version_the_daemon_dropped_takes_its_code_with_it() {
        let mut m = loaded();
        ask(&mut m, 1, "flow", "refresh");
        let v2 = fixture::demo_diagrams(NOW, &pr()).remove(1);
        m.apply(&DiagramsTell::Loaded {
            pr: pr(),
            versions: vec![v2.info()],
            latest: Some(Box::new(v2.clone())),
            state: DiagramsState::Idle,
            head: v2.commit.clone(),
            stale: false,
            stale_reason: None,
        });
        assert!(m.code.is_empty());
    }

    #[test]
    fn the_code_of_a_long_session_does_not_pile_up() {
        let mut m = loaded();
        for n in 0..(MAX_CODES + 5) {
            m.apply(&DiagramsTell::Code {
                pr: pr(),
                number: 2,
                key: "flow".into(),
                node: format!("box{n}"),
                code: answer(1),
            });
        }
        assert!(m.code.len() <= MAX_CODES);
        assert!(
            m.code
                .contains_key(&(2, "flow".to_string(), format!("box{}", MAX_CODES + 4)))
        );
    }

    #[test]
    fn the_selection_is_kept_then_falls_back_to_the_first_ready_entry() {
        let m = loaded();
        let (info, latest) = m.shown(None).unwrap();
        assert!(latest);
        assert_eq!(
            selected_key(info, Some("sequence")).as_deref(),
            Some("sequence")
        );
        assert_eq!(selected_key(info, None).as_deref(), Some("flow"));
        assert_eq!(selected_key(info, Some("gone")).as_deref(), Some("flow"));
        let mut waiting = info.clone();
        waiting.entries[0].state = EntryStateName::Waiting;
        assert_eq!(selected_key(&waiting, None).as_deref(), Some("deps"));
        for e in &mut waiting.entries {
            e.state = EntryStateName::Waiting;
        }
        assert_eq!(selected_key(&waiting, None).as_deref(), Some("flow"));
    }

    #[test]
    fn an_unknown_version_shows_the_latest() {
        let m = loaded();
        assert_eq!(
            m.shown(Some(1)).map(|(i, l)| (i.number, l)),
            Some((1, false))
        );
        assert_eq!(
            m.shown(Some(7)).map(|(i, l)| (i.number, l)),
            Some((2, true))
        );
        assert!(DiagramsModel::default().shown(None).is_none());
    }

    #[test]
    fn compare_needs_an_older_version_that_drew_the_same_diagram() {
        let m = loaded();
        assert_eq!(m.compare_with(2, "flow"), Some(1));
        assert_eq!(m.compare_with(2, "sequence"), Some(1));
        assert_eq!(m.compare_with(2, "deps"), None, "new in version 2");
        assert_eq!(
            m.compare_with(1, "flow"),
            None,
            "version 1 has nothing before it"
        );
        assert_eq!(m.compare_with(2, "nothing"), None);
    }

    #[test]
    fn picked_needs_the_version_whole() {
        let mut m = loaded();
        let mut ui = TabUi::default();
        let p = picked(&m, &ui).expect("the latest is loaded");
        assert_eq!(
            (p.number, p.latest, p.entry.plan.key.as_str()),
            (2, true, "flow")
        );
        ui.diagram = Some("er".into());
        assert_eq!(picked(&m, &ui).unwrap().entry.plan.key, "er");
        ui.diagram_version = Some(1);
        assert!(picked(&m, &ui).is_none(), "version 1 was not asked for yet");
        m.loaded
            .insert(1, fixture::demo_diagrams(NOW, &pr()).remove(0));
        let p = picked(&m, &ui).expect("loaded now");
        assert_eq!((p.number, p.latest), (1, false));
        assert_eq!(
            p.entry.plan.key, "flow",
            "er is not in version 1: the first ready entry"
        );
    }

    #[test]
    fn diagrams_apply_creates_the_model_of_its_review() {
        let mut all = Diagrams::default();
        all.apply(&DiagramsTell::State {
            pr: pr(),
            state: DiagramsState::Planning,
        });
        assert!(all.0[&pr()].busy());
    }
}

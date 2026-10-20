//! The parsed review caches (`cache/reviews/*.json`), kept in memory so an open or a draft edit
//! does not parse a file of up to a few hundred kilobytes again, and the file writes that follow
//! them (the cache files and each worktree's `.clusia/review.md`), done on the blocking pool.
//!
//! While the daemon runs, memory is the source of truth: a change lands there at once and its
//! write is queued. Writes to one file reach it in the order they were queued: a write that
//! finds a later one already on disk is skipped. A request is answered only once the writes
//! queued before its answer are done ([`ReviewCaches::settled`]), and a shutdown waits for them
//! the same way.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use clusia_core::{Paths, PrRef, ReviewCache};
use clusia_store::{delete_review_cache, load_review_cache, save_review_cache};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How many reviews' caches stay in memory, the least recently used dropped first, and the
/// reviews known to have no cache file before any of them. A review whose write is still
/// running is never dropped, so the count can go over this for a moment.
pub(crate) const KEPT: usize = 8;

struct Entry {
    /// `None`: there is no cache file.
    cache: Option<Arc<ReviewCache>>,
    used: u64,
    /// Writes of this cache queued and not done yet.
    writing: usize,
}

#[derive(Default)]
struct Kept {
    entries: HashMap<PrRef, Entry>,
    clock: u64,
    /// Bumped on every change, so a file read that raced a change is not kept.
    changes: u64,
}

impl Kept {
    fn touch(&mut self, pr: &PrRef) -> Option<Option<Arc<ReviewCache>>> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(pr)?;
        entry.used = clock;
        Some(entry.cache.clone())
    }

    fn insert(&mut self, pr: &PrRef, cache: Option<Arc<ReviewCache>>, writing: usize) {
        self.clock += 1;
        let used = self.clock;
        let entry = self.entries.entry(pr.clone()).or_insert(Entry {
            cache: None,
            used,
            writing: 0,
        });
        entry.cache = cache;
        entry.used = used;
        entry.writing += writing;
        while self.entries.len() > KEPT {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, e)| e.writing == 0)
                .min_by_key(|(_, e)| (e.cache.is_some(), e.used))
                .map(|(pr, _)| pr.clone());
            match oldest {
                Some(pr) => self.entries.remove(&pr),
                None => break,
            };
        }
    }
}

#[derive(Default)]
struct Flight {
    /// The ticket the next write gets; tickets start at 1, so 0 means "nothing written".
    next: u64,
    running: BTreeSet<u64>,
}

pub(crate) struct ReviewCaches {
    paths: Paths,
    kept: Mutex<Kept>,
    flight: Mutex<Flight>,
    /// Bumped each time a write ends.
    ended: watch::Sender<u64>,
    /// The ticket of the last write that reached each file. Held while a file is written.
    written: Mutex<HashMap<PathBuf, Arc<Mutex<u64>>>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Ends a write's ticket however its job returns.
struct Ending {
    caches: Arc<ReviewCaches>,
    ticket: u64,
    pr: Option<PrRef>,
}

impl Drop for Ending {
    fn drop(&mut self) {
        if let Some(pr) = &self.pr
            && let Some(entry) = locked(&self.caches.kept).entries.get_mut(pr)
        {
            entry.writing -= 1;
        }
        locked(&self.caches.flight).running.remove(&self.ticket);
        self.caches.ended.send_modify(|n| *n += 1);
    }
}

impl ReviewCaches {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            kept: Mutex::default(),
            flight: Mutex::new(Flight {
                next: 1,
                running: BTreeSet::new(),
            }),
            ended: watch::channel(0).0,
            written: Mutex::default(),
        }
    }

    /// The cache of `pr`; its file is read (on the blocking pool) only when memory has no word
    /// on it.
    pub async fn get(self: &Arc<Self>, pr: &PrRef) -> Option<Arc<ReviewCache>> {
        if let Some(kept) = locked(&self.kept).touch(pr) {
            return kept;
        }
        let this = self.clone();
        let pr = pr.clone();
        tokio::task::spawn_blocking(move || this.get_blocking(&pr))
            .await
            .unwrap_or(None)
    }

    /// [`Self::get`] for code that already runs off the async runtime.
    pub fn get_blocking(&self, pr: &PrRef) -> Option<Arc<ReviewCache>> {
        let before = {
            let mut kept = locked(&self.kept);
            if let Some(cache) = kept.touch(pr) {
                return cache;
            }
            kept.changes
        };
        // A cache that is not in memory has no write queued (those are never dropped), so the
        // file holds the last write that worked; after a failed one that is an older copy.
        let loaded = match load_review_cache(&self.paths, pr) {
            Ok(cache) => cache.map(Arc::new),
            Err(e) => {
                tracing::warn!(error = %e, pr = %pr, "cannot read the review cache");
                return None;
            }
        };
        let mut kept = locked(&self.kept);
        if kept.changes == before {
            kept.insert(pr, loaded.clone(), 0);
        } else if let Some(cache) = kept.touch(pr) {
            return cache;
        }
        loaded
    }

    /// Makes `cache` the cache of `pr` now and queues the write of its file. The returned task
    /// says whether the write worked; a failure is also logged, and memory keeps `cache`.
    pub fn put(
        self: &Arc<Self>,
        pr: &PrRef,
        cache: Arc<ReviewCache>,
    ) -> JoinHandle<io::Result<()>> {
        let ticket = {
            let mut kept = locked(&self.kept);
            kept.changes += 1;
            kept.insert(pr, Some(cache.clone()), 1);
            self.begin()
        };
        let paths = self.paths.clone();
        let target = paths.review_cache_file(pr);
        let pr = pr.clone();
        self.spawn_write(ticket, target, Some(pr.clone()), move || {
            save_review_cache(&paths, &pr, &cache).inspect_err(
                |e| tracing::warn!(error = %e, pr = %pr, "cannot write the review cache"),
            )
        })
    }

    /// Forgets the cache of `pr` and queues the removal of its file.
    pub fn forget(self: &Arc<Self>, pr: &PrRef) -> JoinHandle<io::Result<()>> {
        let ticket = {
            let mut kept = locked(&self.kept);
            kept.changes += 1;
            kept.insert(pr, None, 1);
            self.begin()
        };
        let paths = self.paths.clone();
        let target = paths.review_cache_file(pr);
        let pr = pr.clone();
        self.spawn_write(ticket, target, Some(pr.clone()), move || {
            delete_review_cache(&paths, &pr).inspect_err(
                |e| tracing::warn!(error = %e, pr = %pr, "cannot delete the review cache"),
            )
        })
    }

    /// Queues `job`, which writes `target`, behind the writes already queued for it.
    pub fn write_later(
        self: &Arc<Self>,
        target: PathBuf,
        job: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> JoinHandle<io::Result<()>> {
        let ticket = self.begin();
        self.spawn_write(ticket, target, None, job)
    }

    /// Waits until every write queued before this call is done.
    pub async fn settled(&self) {
        let mut ended = self.ended.subscribe();
        let upto = locked(&self.flight).next;
        loop {
            let oldest = locked(&self.flight).running.first().copied();
            if oldest.is_none_or(|t| t >= upto) {
                return;
            }
            if ended.changed().await.is_err() {
                return;
            }
        }
    }

    fn begin(&self) -> u64 {
        let mut flight = locked(&self.flight);
        let ticket = flight.next;
        flight.next += 1;
        flight.running.insert(ticket);
        ticket
    }

    fn spawn_write(
        self: &Arc<Self>,
        ticket: u64,
        target: PathBuf,
        pr: Option<PrRef>,
        job: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> JoinHandle<io::Result<()>> {
        let ending = Ending {
            caches: self.clone(),
            ticket,
            pr,
        };
        tokio::task::spawn_blocking(move || {
            let slot = locked(&ending.caches.written)
                .entry(target)
                .or_default()
                .clone();
            let mut written = locked(&slot);
            if *written > ticket {
                return Ok(());
            }
            job()?;
            *written = ticket;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::{FileDiff, PrConversation, PrDetail, PrSummary, Role};

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    fn cache(n: u64, fetched_at: i64) -> ReviewCache {
        ReviewCache {
            pr: PrDetail {
                summary: PrSummary {
                    pr: pr(n),
                    title: "Fix cache".into(),
                    author: "maria".into(),
                    url: format!("https://github.com/acme/widgets/pull/{n}"),
                    draft: false,
                    updated_at: "2026-10-01T12:00:00Z".into(),
                    comments: 0,
                },
                base_ref: "main".into(),
                head_ref: "fix".into(),
                base_sha: "b".repeat(40),
                head_sha: "h".repeat(40),
                additions: 1,
                deletions: 0,
                changed_files: 1,
                clone_url: "https://github.com/acme/widgets.git".into(),
                closed: false,
                merged: false,
                body: String::new(),
            },
            files: vec![FileDiff {
                path: "src/a.rs".into(),
                previous_path: None,
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                patch: Some("@@ -1 +1 @@\n-a\n+b".into()),
            }],
            conversation: PrConversation::default(),
            checks: None,
            role: Role::Reviewer,
            viewer: Some("octo".into()),
            worktree: None,
            fetched_at,
        }
    }

    fn caches() -> (tempfile::TempDir, Arc<ReviewCaches>) {
        let dir = tempfile::tempdir().unwrap();
        let caches = Arc::new(ReviewCaches::new(Paths::new(dir.path())));
        (dir, caches)
    }

    fn on_disk(caches: &ReviewCaches, n: u64) -> Option<i64> {
        load_review_cache(&caches.paths, &pr(n))
            .unwrap()
            .map(|c| c.fetched_at)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_last_of_many_racing_writes_is_on_disk_and_in_memory() {
        let (_dir, caches) = caches();
        let writes: Vec<_> = (1..=40)
            .map(|i| caches.put(&pr(7), Arc::new(cache(7, i))))
            .collect();
        for write in writes {
            write.await.unwrap().unwrap();
        }
        assert_eq!(on_disk(&caches, 7), Some(40));
        assert_eq!(caches.get(&pr(7)).await.unwrap().fetched_at, 40);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_removal_queued_after_a_write_wins() {
        let (_dir, caches) = caches();
        let write = caches.put(&pr(7), Arc::new(cache(7, 1)));
        let removal = caches.forget(&pr(7));
        caches.settled().await;
        write.await.unwrap().unwrap();
        removal.await.unwrap().unwrap();
        assert_eq!(on_disk(&caches, 7), None);
        assert_eq!(caches.get(&pr(7)).await, None);
    }

    #[tokio::test]
    async fn a_failed_write_is_reported_and_memory_keeps_the_new_copy() {
        let (_dir, caches) = caches();
        caches
            .put(&pr(7), Arc::new(cache(7, 1)))
            .await
            .unwrap()
            .unwrap();
        // A file where the folder of the caches should be: nothing can be written there.
        let folder = caches.paths.review_cache_file(&pr(7));
        let folder = folder.parent().unwrap();
        let before = std::fs::read(folder.join("acme~widgets~7.json")).unwrap();
        std::fs::rename(folder, folder.with_extension("moved")).unwrap();
        std::fs::write(folder, "not a folder").unwrap();

        let failed = caches.put(&pr(7), Arc::new(cache(7, 2))).await.unwrap();
        assert!(failed.is_err());
        let kept = caches.get(&pr(7)).await.unwrap();
        assert_eq!(*kept, cache(7, 2));

        std::fs::remove_file(folder).unwrap();
        std::fs::rename(folder.with_extension("moved"), folder).unwrap();
        assert_eq!(
            std::fs::read(folder.join("acme~widgets~7.json")).unwrap(),
            before,
            "the file is as it was before the failed write"
        );
    }

    #[tokio::test]
    async fn a_file_is_parsed_once_then_served_from_memory() {
        let (_dir, caches) = caches();
        save_review_cache(&caches.paths, &pr(7), &cache(7, 5)).unwrap();
        for _ in 0..3 {
            assert_eq!(caches.get(&pr(7)).await.unwrap().fetched_at, 5);
        }
        assert_eq!(clusia_store::review_cache_parses(&caches.paths, &pr(7)), 1);
    }

    #[tokio::test]
    async fn only_the_most_recently_used_caches_are_kept() {
        let (_dir, caches) = caches();
        for n in 1..=KEPT as u64 + 1 {
            caches
                .put(&pr(n), Arc::new(cache(n, 1)))
                .await
                .unwrap()
                .unwrap();
        }
        let kept = locked(&caches.kept);
        assert_eq!(kept.entries.len(), KEPT);
        assert!(!kept.entries.contains_key(&pr(1)), "the oldest went first");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_earlier_write_that_reaches_its_file_after_a_later_one_is_skipped() {
        let (dir, caches) = caches();
        let target = dir.path().join("notes.md");
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (started, has_started) = std::sync::mpsc::channel::<()>();
        // The first write is held before it reaches the file, so the second one gets there first.
        let held = caches.write_later(dir.path().join("gate"), move || {
            started.send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        });
        has_started.recv().unwrap();
        let file = target.clone();
        let first = {
            let caches = caches.clone();
            let ticket = caches.begin();
            let gate = held;
            tokio::spawn(async move {
                gate.await.unwrap().unwrap();
                caches
                    .spawn_write(ticket, file.clone(), None, move || {
                        std::fs::write(&file, "first")
                    })
                    .await
                    .unwrap()
            })
        };
        let file = target.clone();
        caches
            .write_later(target.clone(), move || std::fs::write(&file, "second"))
            .await
            .unwrap()
            .unwrap();
        go.send(()).unwrap();
        first.await.unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
    }

    #[tokio::test]
    async fn caches_known_to_be_missing_are_dropped_before_real_ones() {
        let (_dir, caches) = caches();
        caches
            .put(&pr(1), Arc::new(cache(1, 1)))
            .await
            .unwrap()
            .unwrap();
        for n in 2..=KEPT as u64 + 1 {
            assert_eq!(caches.get(&pr(n)).await, None);
        }
        assert!(locked(&caches.kept).entries.contains_key(&pr(1)));
    }

    #[tokio::test]
    async fn settled_waits_for_the_writes_queued_before_it() {
        let (dir, caches) = caches();
        let target = dir.path().join("notes.md");
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let file = target.clone();
        let write = caches.write_later(target.clone(), move || {
            wait.recv().unwrap();
            std::fs::write(&file, "done")
        });
        let settled = tokio::spawn({
            let caches = caches.clone();
            async move { caches.settled().await }
        });
        tokio::task::yield_now().await;
        assert!(!settled.is_finished());
        go.send(()).unwrap();
        settled.await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "done");
        write.await.unwrap().unwrap();
    }
}

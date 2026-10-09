//! `cache/reviews/<owner~repo~n>.json`: what the last successful open fetched, so the window
//! can show a review without the network ("Open from cache"). Losing it only costs a refetch.

use std::fs;
use std::io;

use clusia_core::{Paths, PrRef, ReviewCache};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

pub fn save_review_cache(paths: &Paths, pr: &PrRef, cache: &ReviewCache) -> io::Result<()> {
    let json = serde_json::to_vec(cache).map_err(io::Error::other)?;
    write_atomic(&paths.review_cache_file(pr), &json)
}

/// `None` when there is no cache, or when it was unreadable (it is then quarantined).
pub fn load_review_cache(paths: &Paths, pr: &PrRef) -> io::Result<Option<ReviewCache>> {
    let path = paths.review_cache_file(pr);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice::<ReviewCache>(&bytes) {
        Ok(cache) => Ok(Some(cache)),
        Err(e) => {
            let moved = quarantine(&path, now_unix())?;
            tracing::warn!(error = %e, file = %moved.display(), "review cache was unreadable and was quarantined");
            Ok(None)
        }
    }
}

/// Removes the cache of `pr`; a missing file is fine.
pub fn delete_review_cache(paths: &Paths, pr: &PrRef) -> io::Result<()> {
    match fs::remove_file(paths.review_cache_file(pr)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::{FileDiff, PrConversation, PrDetail, PrSummary, Role};

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn cache() -> ReviewCache {
        ReviewCache {
            pr: PrDetail {
                summary: PrSummary {
                    pr: pr(),
                    title: "Fix cache".into(),
                    author: "maria".into(),
                    url: "https://github.com/acme/widgets/pull/7".into(),
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
            worktree: Some("/w/acme~widgets~7".into()),
            fetched_at: 1_700_000_000,
        }
    }

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        (dir, p)
    }

    #[test]
    fn save_load_delete_round_trip() {
        let (_d, p) = paths();
        assert_eq!(load_review_cache(&p, &pr()).unwrap(), None);
        save_review_cache(&p, &pr(), &cache()).unwrap();
        assert!(p.review_cache_file(&pr()).exists());
        assert_eq!(load_review_cache(&p, &pr()).unwrap(), Some(cache()));
        delete_review_cache(&p, &pr()).unwrap();
        assert!(!p.review_cache_file(&pr()).exists());
        delete_review_cache(&p, &pr()).unwrap();
    }

    #[test]
    fn unreadable_cache_is_quarantined() {
        let (_d, p) = paths();
        let file = p.review_cache_file(&pr());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{ not json").unwrap();
        assert_eq!(load_review_cache(&p, &pr()).unwrap(), None);
        assert!(!file.exists());
        let moved: Vec<String> = fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            moved
                .iter()
                .any(|n| n.starts_with("acme~widgets~7.json.corrupt-")),
            "{moved:?}"
        );
    }
}

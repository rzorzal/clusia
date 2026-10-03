//! `reviews/<owner~repo~n>.json`: one file per review in progress.

use std::fs;
use std::io;
use std::path::PathBuf;

use clusia_core::{Paths, PrRef, Review};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

#[derive(Debug)]
pub enum ReviewLoad {
    Missing,
    Found(Review),
    /// The file was unreadable and was moved aside.
    Quarantined {
        path: PathBuf,
        error: String,
    },
}

pub fn save_review(paths: &Paths, review: &Review) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(review).map_err(io::Error::other)?;
    write_atomic(&paths.review_file(&review.pr), &json)
}

fn load_file(path: &std::path::Path) -> io::Result<ReviewLoad> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ReviewLoad::Missing),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice::<Review>(&bytes) {
        Ok(review) => Ok(ReviewLoad::Found(review)),
        Err(e) => {
            let moved = quarantine(path, now_unix())?;
            tracing::warn!(error = %e, file = %moved.display(), "review file was unreadable and was quarantined");
            Ok(ReviewLoad::Quarantined {
                path: moved,
                error: e.to_string(),
            })
        }
    }
}

pub fn load_review(paths: &Paths, pr: &PrRef) -> io::Result<ReviewLoad> {
    load_file(&paths.review_file(pr))
}

pub fn delete_review(paths: &Paths, pr: &PrRef) -> io::Result<bool> {
    match fs::remove_file(paths.review_file(pr)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn list_reviews(paths: &Paths) -> io::Result<Vec<Review>> {
    let entries = match fs::read_dir(paths.reviews_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut reviews = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let ReviewLoad::Found(review) = load_file(&path)? {
            reviews.push(review);
        }
    }
    reviews.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    Ok(reviews)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        (dir, p)
    }

    fn review(n: u64, updated: i64) -> Review {
        let mut r = Review::new(
            format!("acme/widgets#{n}").parse().unwrap(),
            "t".into(),
            "b".into(),
            "h".into(),
            1,
        );
        r.updated_at = updated;
        r
    }

    #[test]
    fn save_load_delete_round_trip() {
        let (_d, p) = paths();
        let r = review(7, 10);
        assert!(matches!(
            load_review(&p, &r.pr).unwrap(),
            ReviewLoad::Missing
        ));
        save_review(&p, &r).unwrap();
        assert!(p.review_file(&r.pr).exists());
        match load_review(&p, &r.pr).unwrap() {
            ReviewLoad::Found(got) => assert_eq!(got, r),
            other => panic!("{other:?}"),
        }
        assert!(delete_review(&p, &r.pr).unwrap());
        assert!(!delete_review(&p, &r.pr).unwrap());
    }

    #[test]
    fn corrupt_review_is_quarantined() {
        let (_d, p) = paths();
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        fs::create_dir_all(p.reviews_dir()).unwrap();
        fs::write(p.review_file(&pr), "{ not json").unwrap();
        match load_review(&p, &pr).unwrap() {
            ReviewLoad::Quarantined { path, error } => {
                assert!(
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".corrupt-")
                );
                assert!(!error.is_empty());
            }
            other => panic!("{other:?}"),
        }
        assert!(!p.review_file(&pr).exists());
    }

    #[test]
    fn list_is_newest_first_and_skips_junk() {
        let (_d, p) = paths();
        assert!(list_reviews(&p).unwrap().is_empty());
        save_review(&p, &review(1, 10)).unwrap();
        save_review(&p, &review(2, 30)).unwrap();
        save_review(&p, &review(3, 20)).unwrap();
        fs::write(p.reviews_dir().join("broken.json"), "nope").unwrap();
        fs::write(p.reviews_dir().join("notes.txt"), "ignored").unwrap();
        let numbers: Vec<u64> = list_reviews(&p)
            .unwrap()
            .iter()
            .map(|r| r.pr.number)
            .collect();
        assert_eq!(numbers, vec![2, 3, 1]);
        assert!(
            !p.reviews_dir().join("broken.json").exists(),
            "junk json is quarantined"
        );
    }
}

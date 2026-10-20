//! Where words appear in a commit: one streamed `git grep` over the commit's tree.

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use clusia_core::keywords::{self, has_word};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::diff::require_object_id;
use crate::run::{GitError, command, redact_credentials};

/// Room for `<sha>:<path>\0<line>\0` before a line's text.
const PREFIX_MAX: usize = 8_192;
/// How much of git's error output is kept.
const STDERR_MAX: u64 = 16 * 1024;
const EXCLUDE_GENERATED: &str = ":(exclude,attr:linguist-generated)";
const EXCLUDE_VENDORED: &str = ":(exclude,attr:linguist-vendored)";

/// How far a search goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepCaps {
    /// The most hits kept for one word; the rest are counted.
    pub per_word: u32,
    /// The most hits kept in all; the search stops there.
    pub total: u32,
    /// How long the search may run; past it the hits so far come back, cut.
    pub budget: Duration,
    /// Files bigger than this are skipped.
    pub max_file_bytes: u64,
    /// Lines longer than this are skipped.
    pub max_line_bytes: usize,
}

impl GrepCaps {
    /// The caps of the keyword search.
    pub fn keywords() -> Self {
        Self {
            per_word: keywords::MAX_HITS_PER_WORD,
            total: keywords::MAX_HITS,
            budget: Duration::from_secs(keywords::SEARCH_BUDGET_SECS),
            max_file_bytes: keywords::MAX_FILE_BYTES,
            max_line_bytes: keywords::MAX_LINE_BYTES,
        }
    }
}

/// One line of the commit holding at least one of the words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepHit {
    pub path: String,
    pub line: u32,
    pub text: String,
    /// Indexes into the searched words of the words on this line that were still under their cap.
    pub words: Vec<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrepResult {
    pub hits: Vec<GrepHit>,
    /// Per word: lines found past its cap, counted and not kept.
    pub overflow: Vec<u32>,
    /// The budget ran out before git finished.
    pub cut: bool,
}

/// A word git can take as one `-e` pattern: git splits a pattern at a line break, and a NUL
/// cannot be an argument.
fn searchable(word: &str) -> bool {
    !word.is_empty() && !word.contains(['\n', '\r', '\0'])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    More,
    Full,
    Eof,
}

/// What a search has found so far; it outlives the search, so a search cut by its budget still
/// gives what it read.
struct Found {
    hits: Vec<GrepHit>,
    stored: Vec<u32>,
    overflow: Vec<u32>,
}

impl Found {
    fn new(words: usize) -> Self {
        Self {
            hits: Vec::new(),
            stored: vec![0; words],
            overflow: vec![0; words],
        }
    }

    /// Files the hit under each matching word with room left and counts it for the others.
    fn add(
        &mut self,
        path: String,
        line: u32,
        text: String,
        words: &[String],
        caps: &GrepCaps,
    ) -> Flow {
        let mut hit_words = Vec::new();
        for (i, word) in words.iter().enumerate() {
            if !searchable(word) || !has_word(&text, word) {
                continue;
            }
            if self.stored[i] < caps.per_word {
                self.stored[i] += 1;
                // The caller checked that every index fits.
                if let Ok(index) = u16::try_from(i) {
                    hit_words.push(index);
                }
            } else {
                self.overflow[i] += 1;
            }
        }
        if !hit_words.is_empty() {
            self.hits.push(GrepHit {
                path,
                line,
                text,
                words: hit_words,
            });
        }
        if self.hits.len() >= caps.total as usize {
            Flow::Full
        } else {
            Flow::More
        }
    }

    fn finish(self, cut: bool) -> GrepResult {
        GrepResult {
            hits: self.hits,
            overflow: self.overflow,
            cut,
        }
    }
}

/// Answers whether a file of the commit is small enough to search.
trait FileSizes {
    async fn fits(&mut self, path: &str) -> Result<bool, GitError>;
}

/// `git cat-file --batch-check`, asked once per file; grep gives a file's lines together, so
/// only the last answer is kept.
struct BatchSizes {
    _child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    sha: String,
    max: u64,
    last: Option<(String, bool)>,
}

pub(crate) fn spawn_error(e: io::Error) -> GitError {
    if e.kind() == io::ErrorKind::NotFound {
        GitError::Missing
    } else {
        GitError::Io(e)
    }
}

impl BatchSizes {
    fn start(repo: &Path, sha: &str, max: u64) -> Result<Self, GitError> {
        let mut child = command(repo)
            .args(["cat-file", "--batch-check"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(spawn_error)?;
        let input = child.stdin.take().ok_or_else(piped_missing)?;
        let output = BufReader::new(child.stdout.take().ok_or_else(piped_missing)?);
        Ok(Self {
            _child: child,
            input,
            output,
            sha: sha.to_string(),
            max,
            last: None,
        })
    }
}

pub(crate) fn piped_missing() -> GitError {
    GitError::Io(io::Error::other("a piped stream of git is missing"))
}

impl FileSizes for BatchSizes {
    async fn fits(&mut self, path: &str) -> Result<bool, GitError> {
        if let Some((last, fits)) = &self.last
            && last == path
        {
            return Ok(*fits);
        }
        // A line break would end the question early and shift every later answer.
        let fits = if path.contains('\n') {
            false
        } else {
            self.input
                .write_all(format!("{}:{path}\n", self.sha).as_bytes())
                .await?;
            self.input.flush().await?;
            let mut answer = Vec::new();
            match next_record(&mut self.output, &mut answer, PREFIX_MAX + 128).await? {
                Record::Line => {}
                Record::TooLong | Record::Eof => {
                    return Err(GitError::Failed {
                        args: "cat-file --batch-check".into(),
                        stderr: "unexpected answer".into(),
                    });
                }
            }
            let answer = String::from_utf8_lossy(&answer);
            let mut parts = answer.splitn(3, ' ');
            matches!(
                (parts.next(), parts.next(), parts.next()),
                (Some(_), Some("blob"), Some(size))
                    if size.parse::<u64>().is_ok_and(|size| size <= self.max)
            )
        };
        self.last = Some((path.to_string(), fits));
        Ok(fits)
    }
}

pub(crate) enum Record {
    Line,
    TooLong,
    Eof,
}

/// Reads up to the next `\n` (or the end) into `record`, never holding more than `limit` bytes:
/// a longer record is consumed and reported as `TooLong`.
pub(crate) async fn next_record<R: AsyncBufRead + Unpin>(
    out: &mut R,
    record: &mut Vec<u8>,
    limit: usize,
) -> io::Result<Record> {
    record.clear();
    let mut too_long = false;
    let mut any = false;
    loop {
        let buf = out.fill_buf().await?;
        if buf.is_empty() {
            return Ok(match (any, too_long) {
                (false, _) => Record::Eof,
                (true, true) => Record::TooLong,
                (true, false) => Record::Line,
            });
        }
        any = true;
        let newline = buf.iter().position(|&b| b == b'\n');
        let take = newline.unwrap_or(buf.len());
        if !too_long {
            if record.len() + take > limit {
                too_long = true;
                record.clear();
            } else {
                record.extend_from_slice(&buf[..take]);
            }
        }
        out.consume(newline.map_or(take, |at| at + 1));
        if newline.is_some() {
            return Ok(if too_long {
                Record::TooLong
            } else {
                Record::Line
            });
        }
    }
}

/// `<sha>:<path>\0<line>\0<text>` as path, line and text.
fn parse_record(record: &[u8], prefix: &[u8]) -> Option<(String, u32, String)> {
    let rest = record.strip_prefix(prefix)?;
    let mut parts = rest.splitn(3, |&b| b == 0);
    let path = std::str::from_utf8(parts.next()?).ok()?;
    let line: u32 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
    let text = String::from_utf8_lossy(parts.next()?);
    let text = text.strip_suffix('\r').unwrap_or(&text).to_string();
    (!path.is_empty() && line > 0).then(|| (path.to_string(), line, text))
}

/// Reads `git grep -z` records into `found` until the stream ends or the total cap is reached.
async fn read_records<R: AsyncBufRead + Unpin>(
    mut out: R,
    sha: &str,
    words: &[String],
    caps: &GrepCaps,
    sizes: &mut impl FileSizes,
    found: &mut Found,
) -> Result<Flow, GitError> {
    let prefix = format!("{sha}:");
    let mut record = Vec::new();
    loop {
        match next_record(&mut out, &mut record, caps.max_line_bytes + PREFIX_MAX).await? {
            Record::Eof => return Ok(Flow::Eof),
            Record::TooLong => continue,
            Record::Line => {}
        }
        let Some((path, line, text)) = parse_record(&record, prefix.as_bytes()) else {
            continue;
        };
        if text.len() > caps.max_line_bytes || !sizes.fits(&path).await? {
            continue;
        }
        if found.add(path, line, text, words, caps) == Flow::Full {
            return Ok(Flow::Full);
        }
    }
}

/// Where each of `words` appears as a whole word in the tree of commit `sha`, with `caps`
/// bounding how much is kept and how long it takes. Dropping the future kills git.
pub async fn grep_words(
    repo: &Path,
    sha: &str,
    words: &[String],
    caps: GrepCaps,
) -> Result<GrepResult, GitError> {
    require_object_id(sha)?;
    if words.len() > usize::from(u16::MAX) {
        return Err(GitError::Failed {
            args: "grep".into(),
            stderr: "too many words".into(),
        });
    }
    let mut found = Found::new(words.len());
    if !words.iter().any(|w| searchable(w)) {
        return Ok(found.finish(false));
    }
    match tokio::time::timeout(caps.budget, scan(repo, sha, words, &caps, &mut found)).await {
        Ok(Ok(())) => Ok(found.finish(false)),
        Ok(Err(e)) => Err(e),
        // The scan was dropped with both children, which `kill_on_drop` kills.
        Err(_) => Ok(found.finish(true)),
    }
}

async fn scan(
    repo: &Path,
    sha: &str,
    words: &[String],
    caps: &GrepCaps,
    found: &mut Found,
) -> Result<(), GitError> {
    let mut args = vec![
        "-c",
        "grep.column=false",
        "grep",
        "-n",
        "-w",
        "-F",
        "-I",
        "-z",
        "--no-color",
    ];
    for word in words.iter().filter(|w| searchable(w)) {
        args.push("-e");
        args.push(word);
    }
    args.extend([sha, "--", ".", EXCLUDE_GENERATED, EXCLUDE_VENDORED]);

    let mut child = command(repo)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let stdout = child.stdout.take().ok_or_else(piped_missing)?;
    let mut stderr = child.stderr.take().ok_or_else(piped_missing)?;
    // Only the start of the error output is kept, but all of it is read so git never blocks.
    let stderr = tokio::spawn(async move {
        let mut kept = Vec::new();
        let _ = (&mut stderr).take(STDERR_MAX).read_to_end(&mut kept).await;
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        kept
    });
    let mut sizes = BatchSizes::start(repo, sha, caps.max_file_bytes)?;

    match read_records(BufReader::new(stdout), sha, words, caps, &mut sizes, found).await? {
        Flow::Full | Flow::More => Ok(()),
        Flow::Eof => {
            let status = child.wait().await?;
            if matches!(status.code(), Some(0 | 1)) {
                return Ok(());
            }
            let stderr = stderr.await.unwrap_or_default();
            Err(GitError::Failed {
                args: "grep".into(),
                stderr: redact_credentials(String::from_utf8_lossy(&stderr).trim()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tokio::io::{AsyncWriteExt, BufReader};

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn caps() -> GrepCaps {
        GrepCaps {
            per_word: 100,
            total: 100,
            budget: Duration::from_secs(10),
            max_file_bytes: 200,
            max_line_bytes: 4_096,
        }
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| (*w).to_string()).collect()
    }

    /// A repository whose one commit holds words in plain, non-UTF-8, binary, big, long-lined,
    /// vendored, generated and linked files; the worktree is then changed without a commit.
    /// Returns the commit.
    fn repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        sh(
            tmp.path(),
            &["init", "-q", "-b", "main", repo.to_str().unwrap()],
        );
        for dir in ["src", "vendor", "gen"] {
            std::fs::create_dir_all(repo.join(dir)).unwrap();
        }
        let files: [(&str, &[u8]); 10] = [
            (
                "src/retry.rs",
                b"pub fn retry_count() {}\nlet retry = 1;\nretrying\n// retry retry\n",
            ),
            (
                "src/lib.rs",
                b"use retry;\nfn backoff() {}\nretry backoff\n",
            ),
            ("src/a\u{e7}\u{e3}o.rs", b"retry\n"),
            ("latin1.txt", b"retry \xe9t\xe9\n"),
            ("vendor/lib.js", b"retry()\n"),
            ("gen/api.rs", b"retry\n"),
            (
                ".gitattributes",
                b"vendor/** linguist-vendored\ngen/** linguist-generated\n",
            ),
            ("bin.dat", b"retry\0retry\n"),
            (
                "long.txt",
                &[b"retry ".as_slice(), &[b'y'; 5_000], b"\n"].concat(),
            ),
            (
                "big.txt",
                &[b"retry\n".as_slice(), &b"x\n".repeat(150)].concat(),
            ),
        ];
        for (path, bytes) in files {
            std::fs::write(repo.join(path), bytes).unwrap();
        }
        std::os::unix::fs::symlink("src/lib.rs", repo.join("link.rs")).unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "one"]);
        let sha = sh(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("src/lib.rs"), "retry\nretry\nretry\nretry\n").unwrap();
        std::fs::write(repo.join("new.rs"), "retry\n").unwrap();
        (tmp, repo, sha)
    }

    fn sorted(mut hits: Vec<GrepHit>) -> Vec<(String, u32, Vec<u16>)> {
        hits.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        hits.into_iter()
            .map(|h| (h.path, h.line, h.words))
            .collect()
    }

    #[test]
    fn the_keyword_caps_are_the_contracts() {
        assert_eq!(
            GrepCaps::keywords(),
            GrepCaps {
                per_word: 2_000,
                total: 20_000,
                budget: Duration::from_secs(10),
                max_file_bytes: 1_048_576,
                max_line_bytes: 4_096,
            }
        );
    }

    #[tokio::test]
    async fn whole_words_of_the_commits_text_files_only() {
        let (_t, repo, sha) = repo();
        let found = grep_words(&repo, &sha, &words(&["retry", "backoff", "lib"]), caps())
            .await
            .unwrap();
        assert!(!found.cut);
        assert_eq!(found.overflow, [0, 0, 0]);
        let text = |path: &str, line: u32| {
            found
                .hits
                .iter()
                .find(|h| h.path == path && h.line == line)
                .map(|h| h.text.clone())
        };
        assert_eq!(text("src/lib.rs", 3).as_deref(), Some("retry backoff"));
        assert_eq!(
            text("latin1.txt", 1).as_deref(),
            Some("retry \u{fffd}t\u{fffd}"),
            "a line that is not UTF-8 is read lossily"
        );
        // Not `retry_count` or `retrying`; not the vendored, generated, binary, big or long-lined
        // files; not the link (its blob is a path); not the worktree's edits or untracked file.
        assert_eq!(
            sorted(found.hits),
            [
                ("latin1.txt".to_string(), 1, vec![0]),
                ("src/a\u{e7}\u{e3}o.rs".to_string(), 1, vec![0]),
                ("src/lib.rs".to_string(), 1, vec![0]),
                ("src/lib.rs".to_string(), 2, vec![1]),
                ("src/lib.rs".to_string(), 3, vec![0, 1]),
                ("src/retry.rs".to_string(), 2, vec![0]),
                ("src/retry.rs".to_string(), 4, vec![0]),
            ]
        );
    }

    #[tokio::test]
    async fn the_size_caps_come_from_the_caller() {
        let (_t, repo, sha) = repo();
        let roomy = GrepCaps {
            max_file_bytes: 10_000,
            max_line_bytes: 6_000,
            ..caps()
        };
        let found = grep_words(&repo, &sha, &words(&["retry"]), roomy)
            .await
            .unwrap();
        let paths: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"big.txt"), "{paths:?}");
        assert!(paths.contains(&"long.txt"), "{paths:?}");
    }

    #[tokio::test]
    async fn words_are_patterns_never_options() {
        let (_t, repo, sha) = repo();
        let marker = repo.join("x");
        let list = words(&[
            "-e",
            &format!("--output={}", marker.display()),
            "-n",
            "retry backoff",
            "retry\nbackoff",
            "",
        ]);
        let found = grep_words(&repo, &sha, &list, caps()).await.unwrap();
        assert!(!marker.exists(), "no option reached git");
        assert_eq!(found.overflow, [0; 6]);
        assert_eq!(
            sorted(found.hits),
            [("src/lib.rs".to_string(), 3, vec![3])],
            "a phrase is one fixed string; a word with a line break is never sent"
        );
        let nothing = grep_words(&repo, &sha, &words(&["", "a\nb"]), caps())
            .await
            .unwrap();
        assert_eq!(
            nothing,
            GrepResult {
                hits: Vec::new(),
                overflow: vec![0, 0],
                cut: false
            }
        );
    }

    #[tokio::test]
    async fn only_a_full_object_id_is_searched() {
        let (_t, repo, _sha) = repo();
        for sha in ["HEAD", "main", "--output=/tmp/x", "-e", ""] {
            assert!(
                grep_words(&repo, sha, &words(&["retry"]), caps())
                    .await
                    .is_err(),
                "{sha}"
            );
        }
        let unknown = "d".repeat(40);
        assert!(matches!(
            grep_words(&repo, &unknown, &words(&["retry"]), caps()).await,
            Err(GitError::Failed { .. })
        ));
    }

    #[tokio::test]
    async fn a_word_past_its_cap_is_counted_not_kept() {
        let (_t, repo, sha) = repo();
        let tight = GrepCaps {
            per_word: 3,
            ..caps()
        };
        let found = grep_words(&repo, &sha, &words(&["retry", "backoff"]), tight)
            .await
            .unwrap();
        let kept = |w: u16| found.hits.iter().filter(|h| h.words.contains(&w)).count();
        // `retry` is on six lines of the commit's text files, `backoff` on two.
        assert_eq!((kept(0), kept(1)), (3, 2));
        assert_eq!(found.overflow, [3, 0]);
        assert!(!found.cut);
    }

    #[tokio::test]
    async fn the_total_cap_stops_the_search() {
        let (_t, repo, sha) = repo();
        let tight = GrepCaps { total: 2, ..caps() };
        let found = grep_words(&repo, &sha, &words(&["retry", "backoff"]), tight)
            .await
            .unwrap();
        assert_eq!(found.hits.len(), 2);
        assert!(!found.cut, "a cap is not the budget");
    }

    /// What this test process started that still exists (running or not yet reaped) and whose
    /// arguments hold `marker`, or that is a git child left as a zombie.
    fn leftovers(marker: &str) -> Vec<String> {
        let me = std::process::id().to_string();
        let out = Command::new("ps")
            .args(["-A", "-o", "ppid=,stat=,args="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| {
                let mut fields = line.split_whitespace();
                if fields.next() != Some(me.as_str()) {
                    return false;
                }
                let stat = fields.next().unwrap_or("");
                let args = fields.collect::<Vec<_>>().join(" ");
                args.contains(marker) || (stat.starts_with('Z') && args.contains("git"))
            })
            .map(str::to_string)
            .collect()
    }

    async fn none_left(marker: &str) {
        let mut left = leftovers(marker);
        for _ in 0..100 {
            if left.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            left = leftovers(marker);
        }
        panic!("git outlived the search: {left:?}");
    }

    #[tokio::test]
    async fn a_search_cut_by_its_budget_or_dropped_leaves_no_git_behind() {
        let (_t, repo, _sha) = repo();
        std::fs::write(repo.join("many.txt"), "retry\n".repeat(200_000)).unwrap();
        sh(&repo, &["add", "many.txt"]);
        sh(&repo, &["commit", "-q", "-m", "many"]);
        let sha = sh(&repo, &["rev-parse", "HEAD"]);
        // Every git this search starts runs as `git -C <repo> …`, so the path finds them.
        let marker = repo.display().to_string();
        let roomy = GrepCaps {
            per_word: 1_000_000,
            total: 1_000_000,
            budget: Duration::from_millis(1),
            max_file_bytes: 10_000_000,
            max_line_bytes: 4_096,
        };
        let cut = grep_words(&repo, &sha, &words(&["retry"]), roomy.clone())
            .await
            .unwrap();
        assert!(cut.cut, "200,000 lines take longer than 1 ms");
        none_left(&marker).await;

        // Dropped mid-way, as a Stop does.
        let slow = GrepCaps {
            budget: Duration::from_secs(60),
            ..roomy
        };
        let list = words(&["retry"]);
        tokio::select! {
            _ = grep_words(&repo, &sha, &list, slow) => {}
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        none_left(&marker).await;
    }

    struct AllFit;

    impl FileSizes for AllFit {
        async fn fits(&mut self, _path: &str) -> Result<bool, GitError> {
            Ok(true)
        }
    }

    const SHA: &str = "0123456789012345678901234567890123456789";

    #[tokio::test]
    async fn the_budget_returns_what_was_read_so_far() {
        let (mut git, out) = tokio::io::duplex(1 << 16);
        let records = format!(
            "{SHA}:a.rs\x001\x00let retry = 1;\n{SHA}:b.rs\x002\x00retry\r\n{SHA}:c.rs\x003\x00retr"
        );
        git.write_all(records.as_bytes()).await.unwrap();
        // `git` stays open: the stream never ends, like a slow repository.
        let list = words(&["retry"]);
        let mut found = Found::new(list.len());
        let read = tokio::time::timeout(
            Duration::from_millis(200),
            read_records(
                BufReader::new(out),
                SHA,
                &list,
                &caps(),
                &mut AllFit,
                &mut found,
            ),
        )
        .await;
        assert!(read.is_err(), "the budget fired");
        let result = found.finish(true);
        assert!(result.cut);
        assert_eq!(
            result
                .hits
                .iter()
                .map(|h| (h.path.as_str(), h.line, h.text.as_str()))
                .collect::<Vec<_>>(),
            [("a.rs", 1, "let retry = 1;"), ("b.rs", 2, "retry")]
        );
        drop(git);
    }

    #[tokio::test]
    async fn records_that_are_too_long_or_malformed_are_skipped() {
        let (mut git, out) = tokio::io::duplex(1 << 20);
        let long = "retry ".repeat(2_000);
        let records = format!(
            "{SHA}:long.rs\x001\x00{long}\n\
             another:x.rs\x001\x00retry\n\
             {SHA}:noline.rs\x00retry\n\
             {SHA}:zero.rs\x000\x00retry\n\
             {SHA}:\x001\x00retry\n\
             {SHA}:none.rs\x001\x00retrying\n\
             {SHA}:ok.rs\x007\x00retry"
        );
        git.write_all(records.as_bytes()).await.unwrap();
        drop(git);
        let list = words(&["retry"]);
        let mut found = Found::new(1);
        let flow = read_records(
            BufReader::new(out),
            SHA,
            &list,
            &caps(),
            &mut AllFit,
            &mut found,
        )
        .await
        .unwrap();
        assert_eq!(flow, Flow::Eof);
        let result = found.finish(false);
        assert_eq!(
            sorted(result.hits),
            [("ok.rs".to_string(), 7, vec![0])],
            "the last record needs no line break"
        );
    }
}

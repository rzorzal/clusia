//! A commit's tree in bulk: its files, their text through one `cat-file --batch`, and the
//! attributes that mark files as vendored or generated, read from the commit itself.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::diff::require_object_id;
use crate::grep::{Record, next_record, piped_missing, spawn_error};
use crate::run::{GitError, command, redact_credentials};

/// The mode of a symbolic link: its blob is a path, not a file's text.
const SYMLINK: &str = "120000";
/// The longest header `cat-file --batch` writes before an object.
const HEADER_MAX: usize = 512;

/// One file of a commit's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    pub path: String,
    pub oid: String,
    pub size: u64,
}

fn failed(args: &str, why: impl Into<String>) -> GitError {
    GitError::Failed {
        args: args.into(),
        stderr: why.into(),
    }
}

/// Every file of commit `sha` (a full object id), by path: links, submodules and paths that are
/// not UTF-8 are left out.
pub async fn ls_tree(repo: &Path, sha: &str) -> Result<Vec<TreeFile>, GitError> {
    require_object_id(sha)?;
    let out = command(repo)
        .args([
            "-c",
            "core.quotePath=false",
            "ls-tree",
            "-r",
            "-z",
            "--long",
            "--full-tree",
            sha,
        ])
        .output()
        .await
        .map_err(spawn_error)?;
    if !out.status.success() {
        return Err(failed(
            "ls-tree",
            redact_credentials(String::from_utf8_lossy(&out.stderr).trim()),
        ));
    }
    Ok(out
        .stdout
        .split(|&b| b == 0)
        .filter_map(parse_entry)
        .collect())
}

/// `<mode> <type> <oid> <size>\t<path>` as a file, when it is a blob that is not a link.
fn parse_entry(record: &[u8]) -> Option<TreeFile> {
    let tab = record.iter().position(|&b| b == b'\t')?;
    let head = std::str::from_utf8(&record[..tab]).ok()?;
    let path = std::str::from_utf8(&record[tab + 1..]).ok()?;
    let mut fields = head.split_whitespace();
    let (mode, kind, oid, size) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    if kind != "blob" || mode == SYMLINK || path.is_empty() {
        return None;
    }
    Some(TreeFile {
        path: path.to_string(),
        oid: oid.to_string(),
        size: size.parse().ok()?,
    })
}

/// The text of each blob of `oids`, in order: `None` for a blob over `max_bytes`, binary or not
/// UTF-8, for a missing object and for anything that is not a blob. One git answers them all;
/// dropping the future kills it.
pub async fn read_blobs(
    repo: &Path,
    oids: &[String],
    max_bytes: u64,
) -> Result<Vec<Option<String>>, GitError> {
    for oid in oids {
        require_object_id(oid)?;
    }
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = command(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(spawn_error)?;
    let mut input = child.stdin.take().ok_or_else(piped_missing)?;
    let mut output = BufReader::new(child.stdout.take().ok_or_else(piped_missing)?);
    // Git answers while it reads, so the questions go in from a task: written all at once
    // first, the two pipes would fill and stall both sides.
    let questions: Vec<u8> = oids
        .iter()
        .flat_map(|oid| format!("{oid}\n").into_bytes())
        .collect();
    let writer = tokio::spawn(async move {
        let _ = input.write_all(&questions).await;
        let _ = input.shutdown().await;
    });
    let mut texts = Vec::with_capacity(oids.len());
    let mut header = Vec::new();
    for oid in oids {
        match next_record(&mut output, &mut header, HEADER_MAX).await? {
            Record::Line => {}
            Record::TooLong | Record::Eof => {
                return Err(failed("cat-file --batch", "an answer is missing"));
            }
        }
        let line = String::from_utf8_lossy(&header).into_owned();
        texts.push(read_one(&mut output, oid, &line, max_bytes).await?);
    }
    let _ = writer.await;
    let status = child.wait().await?;
    if !status.success() {
        return Err(failed(
            "cat-file --batch",
            format!("git ended with {status}"),
        ));
    }
    Ok(texts)
}

/// The object after `header` (`<oid> <type> <size>`, or `<oid> missing`).
async fn read_one<R: AsyncBufRead + Unpin>(
    out: &mut R,
    oid: &str,
    header: &str,
    max: u64,
) -> Result<Option<String>, GitError> {
    let mut parts = header.split(' ');
    if !parts
        .next()
        .is_some_and(|said| said.eq_ignore_ascii_case(oid))
    {
        return Err(failed(
            "cat-file --batch",
            format!("an answer for another object: {header}"),
        ));
    }
    let (Some(kind), Some(size)) = (parts.next(), parts.next()) else {
        // `<oid> missing`: nothing follows.
        return Ok(None);
    };
    let size: u64 = size.parse().map_err(|_| {
        failed(
            "cat-file --batch",
            format!("an unreadable answer: {header}"),
        )
    })?;
    if kind != "blob" || size > max {
        skip(out, size + 1).await?;
        return Ok(None);
    }
    let len = usize::try_from(size + 1).map_err(|_| failed("cat-file --batch", "too big"))?;
    let mut bytes = vec![0; len];
    out.read_exact(&mut bytes).await?;
    // The line break git writes after every object.
    bytes.pop();
    if bytes.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

/// Reads and drops `count` bytes.
async fn skip<R: AsyncRead + Unpin>(out: &mut R, count: u64) -> io::Result<()> {
    let dropped = tokio::io::copy(&mut out.take(count), &mut tokio::io::sink()).await?;
    if dropped < count {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    Ok(())
}

/// The paths of `paths` that commit `sha` marks `linguist-vendored` or `linguist-generated`,
/// with the attributes read from the commit, not the worktree (`check-attr --source`, git 2.40).
pub async fn vendored_or_generated(
    repo: &Path,
    sha: &str,
    paths: &[String],
) -> Result<HashSet<String>, GitError> {
    require_object_id(sha)?;
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let source = format!("--source={sha}");
    let mut child = command(repo)
        .args([
            "check-attr",
            &source,
            "--stdin",
            "-z",
            "linguist-vendored",
            "linguist-generated",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let mut input = child.stdin.take().ok_or_else(piped_missing)?;
    let list: Vec<u8> = paths
        .iter()
        .filter(|p| !p.contains('\0'))
        .flat_map(|p| [p.as_bytes(), b"\0"].concat())
        .collect();
    let writer = tokio::spawn(async move {
        let _ = input.write_all(&list).await;
        let _ = input.shutdown().await;
    });
    let out = child.wait_with_output().await?;
    let _ = writer.await;
    if !out.status.success() {
        return Err(failed(
            "check-attr",
            redact_credentials(String::from_utf8_lossy(&out.stderr).trim()),
        ));
    }
    let fields: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
    Ok(fields
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|f| matches!(f[2], b"set" | b"true"))
        .filter_map(|f| std::str::from_utf8(f[0]).ok().map(str::to_string))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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

    /// A repository whose one commit holds text, binary, non-UTF-8, big, vendored and generated
    /// files and a link, with attributes that mark `vendor/` and `gen/`. Returns the commit.
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
        let files: [(&str, &[u8]); 8] = [
            ("src/a.rs", b"fn a() {}\n"),
            ("src/b.rs", b"fn b() {\n    a();\n}\n"),
            ("vendor/v.js", b"v()\n"),
            ("gen/g.rs", b"fn g() {}\n"),
            (
                ".gitattributes",
                b"vendor/** linguist-vendored\ngen/** linguist-generated\nsrc/a.rs linguist-generated=false\n",
            ),
            ("bin.dat", b"a\0b"),
            ("latin1.txt", b"caf\xe9\n"),
            ("big.txt", &[b'x'; 100]),
        ];
        for (path, bytes) in files {
            std::fs::write(repo.join(path), bytes).unwrap();
        }
        std::os::unix::fs::symlink("src/a.rs", repo.join("link.rs")).unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "one"]);
        let sha = sh(&repo, &["rev-parse", "HEAD"]);
        (tmp, repo, sha)
    }

    fn oid(repo: &Path, sha: &str, path: &str) -> String {
        sh(repo, &["rev-parse", &format!("{sha}:{path}")])
    }

    #[tokio::test]
    async fn the_tree_lists_every_blob_with_its_size_and_leaves_links_out() {
        let (_t, repo, sha) = repo();
        let files = ls_tree(&repo, &sha).await.unwrap();
        let listed: Vec<(&str, u64)> = files.iter().map(|f| (f.path.as_str(), f.size)).collect();
        assert_eq!(
            listed,
            [
                (".gitattributes", 88),
                ("big.txt", 100),
                ("bin.dat", 3),
                ("gen/g.rs", 10),
                ("latin1.txt", 5),
                ("src/a.rs", 10),
                ("src/b.rs", 20),
                ("vendor/v.js", 4),
            ]
        );
        assert_eq!(files[5].oid, oid(&repo, &sha, "src/a.rs"));
    }

    #[tokio::test]
    async fn blobs_come_back_in_the_order_asked_and_only_as_small_utf8_text() {
        let (_t, repo, sha) = repo();
        let asked = vec![
            oid(&repo, &sha, "src/b.rs"),
            oid(&repo, &sha, "bin.dat"),
            oid(&repo, &sha, "big.txt"),
            oid(&repo, &sha, "latin1.txt"),
            "f".repeat(40),
            oid(&repo, &sha, "src"),
            sha.clone(),
            oid(&repo, &sha, "src/a.rs"),
        ];
        let texts = read_blobs(&repo, &asked, 50).await.unwrap();
        assert_eq!(
            texts,
            [
                Some("fn b() {\n    a();\n}\n".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                Some("fn a() {}\n".to_string()),
            ],
            "binary, too big, not UTF-8, missing, a tree and a commit are None"
        );
        assert_eq!(
            read_blobs(&repo, &[oid(&repo, &sha, "big.txt")], 100)
                .await
                .unwrap(),
            [Some("x".repeat(100))]
        );
        assert!(read_blobs(&repo, &[], 50).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn thousands_of_blobs_are_read_by_one_git_without_a_stall() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        sh(
            tmp.path(),
            &["init", "-q", "-b", "main", repo.to_str().unwrap()],
        );
        std::fs::create_dir_all(repo.join("many")).unwrap();
        for i in 0..3_000 {
            std::fs::write(
                repo.join(format!("many/f{i:04}.rs")),
                format!("fn f{i}() {{}}\n{}\n", "x".repeat(200)),
            )
            .unwrap();
        }
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "many"]);
        let sha = sh(&repo, &["rev-parse", "HEAD"]);
        let files = ls_tree(&repo, &sha).await.unwrap();
        assert_eq!(files.len(), 3_000);
        let oids: Vec<String> = files.iter().map(|f| f.oid.clone()).collect();
        let texts = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            read_blobs(&repo, &oids, 1_000),
        )
        .await
        .expect("no stall")
        .unwrap();
        for (file, text) in files.iter().zip(&texts) {
            let n: usize = file.path["many/f".len()..file.path.len() - 3]
                .parse()
                .unwrap();
            assert_eq!(
                text.as_deref(),
                Some(format!("fn f{n}() {{}}\n{}\n", "x".repeat(200)).as_str())
            );
        }
    }

    #[tokio::test]
    async fn attributes_are_read_from_the_commit_not_the_worktree() {
        let (_t, repo, sha) = repo();
        // An attribute written after the commit does not count.
        std::fs::write(repo.join(".gitattributes"), "src/** linguist-generated\n").unwrap();
        let paths: Vec<String> = [
            "src/a.rs",
            "src/b.rs",
            "vendor/v.js",
            "gen/g.rs",
            "nope/x.rs",
        ]
        .iter()
        .map(|p| (*p).to_string())
        .collect();
        let marked = vendored_or_generated(&repo, &sha, &paths).await.unwrap();
        let mut marked: Vec<&str> = marked.iter().map(String::as_str).collect();
        marked.sort_unstable();
        assert_eq!(marked, ["gen/g.rs", "vendor/v.js"]);
        assert!(
            vendored_or_generated(&repo, &sha, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn only_full_object_ids_reach_git() {
        let (_t, repo, _sha) = repo();
        for bad in ["HEAD", "main", "--output=/tmp/x", "-e", ""] {
            assert!(ls_tree(&repo, bad).await.is_err(), "{bad}");
            assert!(
                read_blobs(&repo, &[bad.to_string()], 10).await.is_err(),
                "{bad}"
            );
            assert!(
                vendored_or_generated(&repo, bad, &["a".to_string()])
                    .await
                    .is_err(),
                "{bad}"
            );
        }
        assert!(matches!(
            ls_tree(&repo, &"d".repeat(40)).await,
            Err(GitError::Failed { .. })
        ));
    }
}

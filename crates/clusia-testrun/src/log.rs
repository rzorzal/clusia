//! Run logs that stay bounded: the start and the end of a long output are kept.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use clusia_core::tests::{LOG_HEAD, LOG_TAIL};

/// Writes the first `head` bytes to its file at once and keeps only the last `tail` bytes after
/// that, in memory, for `finish` to append behind a line that says how much was cut.
pub struct CappedLog {
    file: BufWriter<File>,
    head_left: usize,
    tail: VecDeque<u8>,
    tail_max: usize,
    cut: u64,
    total: u64,
}

impl CappedLog {
    pub fn create(path: &Path) -> io::Result<Self> {
        Self::with_limits(path, LOG_HEAD, LOG_TAIL)
    }

    pub(crate) fn with_limits(path: &Path, head: usize, tail: usize) -> io::Result<Self> {
        Ok(Self {
            file: BufWriter::new(File::create(path)?),
            head_left: head,
            tail: VecDeque::new(),
            tail_max: tail,
            cut: 0,
            total: 0,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.total += bytes.len() as u64;
        let (head, rest) = bytes.split_at(self.head_left.min(bytes.len()));
        if !head.is_empty() {
            self.file.write_all(head)?;
            self.head_left -= head.len();
        }
        // Only the last `tail_max` bytes of a big piece can survive.
        let kept = &rest[rest.len().saturating_sub(self.tail_max)..];
        self.cut += (rest.len() - kept.len()) as u64;
        self.tail.extend(kept);
        let over = self.tail.len().saturating_sub(self.tail_max);
        self.tail.drain(..over);
        self.cut += over as u64;
        Ok(())
    }

    /// Completes the file and returns how many bytes were written to it, before any cut.
    pub fn finish(mut self) -> io::Result<u64> {
        if self.cut > 0 {
            write!(self.file, "\n… {} bytes cut …\n", self.cut)?;
        }
        let (a, b) = self.tail.as_slices();
        self.file.write_all(a)?;
        self.file.write_all(b)?;
        self.file.flush()?;
        Ok(self.total)
    }
}

/// The last bytes of an output.
pub struct Tail {
    buf: VecDeque<u8>,
    max: usize,
}

impl Tail {
    pub fn new(max: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            max,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf
            .extend(&bytes[bytes.len().saturating_sub(self.max)..]);
        let over = self.buf.len().saturating_sub(self.max);
        self.buf.drain(..over);
    }

    /// Starts at a character boundary: the continuation bytes of a character the cut went
    /// through are dropped. Anything else that is not UTF-8 becomes U+FFFD.
    pub fn text(&self) -> String {
        let bytes: Vec<u8> = self.buf.iter().copied().collect();
        let start = bytes
            .iter()
            .position(|b| b & 0xC0 != 0x80)
            .unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        String::from_utf8(std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn a_log_that_fits_is_the_bytes_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        let mut log = CappedLog::with_limits(&path, 8, 8).unwrap();
        log.write(b"abc").unwrap();
        log.write(b"de").unwrap();
        assert_eq!(log.finish().unwrap(), 5);
        assert_eq!(read(&path), "abcde");
    }

    #[test]
    fn a_long_log_keeps_its_head_and_tail_and_says_what_it_cut() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        let mut log = CappedLog::with_limits(&path, 4, 4).unwrap();
        for piece in ["abcdef", "ghij", "klmnop"] {
            log.write(piece.as_bytes()).unwrap();
        }
        assert_eq!(log.finish().unwrap(), 16);
        assert_eq!(read(&path), "abcd\n… 8 bytes cut …\nmnop");
    }

    #[test]
    fn the_cut_may_fall_inside_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        let mut log = CappedLog::with_limits(&path, 2, 3).unwrap();
        log.write(b"0123456789").unwrap();
        log.finish().unwrap();
        assert_eq!(read(&path), "01\n… 5 bytes cut …\n789");
    }

    #[test]
    fn the_real_limits_are_two_and_eight_mebibytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        let mut log = CappedLog::create(&path).unwrap();
        let block = vec![b'x'; 1024 * 1024];
        for _ in 0..12 {
            log.write(&block).unwrap();
        }
        assert_eq!(log.finish().unwrap(), 12 * 1024 * 1024);
        let text = read(&path);
        let cut = 12 * 1024 * 1024 - 10 * 1024 * 1024;
        let marker = format!("\n… {cut} bytes cut …\n");
        assert!(text.contains(&marker));
        assert_eq!(text.len(), 10 * 1024 * 1024 + marker.len());
    }

    #[test]
    fn a_tail_keeps_the_last_bytes() {
        let mut tail = Tail::new(5);
        tail.push(b"hello ");
        tail.push(b"world");
        assert_eq!(tail.text(), "world");
        assert_eq!(Tail::new(5).text(), "");
    }

    #[test]
    fn a_tail_never_starts_inside_a_character() {
        let mut tail = Tail::new(4);
        tail.push("aé✓".as_bytes());
        // "aé✓" is 61 C3 A9 E2 9C 93: the last 4 bytes start with the continuation byte A9 of "é".
        assert_eq!(tail.text(), "✓");
        let mut tail = Tail::new(3);
        tail.push("é✓".as_bytes());
        assert_eq!(tail.text(), "✓");
    }
}

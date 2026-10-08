//! Keeps `clusia-suggestion` blocks out of the text that streams to the chat. A block that
//! parses becomes a suggestion card at the end of the turn; one that does not stays text.

use clusia_harness::extract_suggestions;

const FENCE: &str = "```clusia-suggestion";

#[derive(Default)]
pub(crate) struct StreamFilter {
    /// The start of the current line, held back while it could still turn into a fence.
    line: String,
    /// The block being collected, from its opening fence on.
    held: Option<String>,
    /// The start of the current line was already let through, so the rest goes too.
    passing: bool,
}

impl StreamFilter {
    /// Takes the next piece of streamed text; returns what is safe to show now.
    pub(crate) fn push(&mut self, delta: &str) -> String {
        let mut out = String::new();
        let mut rest = delta;
        while let Some(i) = rest.find('\n') {
            let (head, tail) = rest.split_at(i + 1);
            self.line.push_str(head);
            let line = std::mem::take(&mut self.line);
            self.whole_line(&line, &mut out);
            rest = tail;
        }
        self.line.push_str(rest);
        self.partial_line(&mut out);
        out
    }

    /// The turn ended: whatever is still held (an unclosed block) is plain text after all.
    pub(crate) fn finish(&mut self) -> String {
        let mut out = self.held.take().unwrap_or_default();
        out.push_str(&std::mem::take(&mut self.line));
        self.passing = false;
        out
    }

    fn whole_line(&mut self, line: &str, out: &mut String) {
        if self.passing {
            self.passing = false;
            out.push_str(line);
        } else if let Some(block) = &mut self.held {
            block.push_str(line);
            if line.trim() == "```" {
                let block = self.held.take().unwrap_or_default();
                if extract_suggestions(&block).1.is_empty() {
                    out.push_str(&block);
                }
            }
        } else if line.trim_start().starts_with(FENCE) {
            self.held = Some(line.to_string());
        } else {
            out.push_str(line);
        }
    }

    fn partial_line(&mut self, out: &mut String) {
        if self.held.is_some() {
            return;
        }
        let start = self.line.trim_start();
        if self.passing || !(FENCE.starts_with(start) || start.starts_with(FENCE)) {
            out.push_str(&std::mem::take(&mut self.line));
            self.passing = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str =
        "```clusia-suggestion\n{\"file\":\"src/a.rs\",\"line\":3,\"body\":\"Why?\"}\n```\n";

    fn run(text: &str, size: usize) -> String {
        let mut filter = StreamFilter::default();
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        for piece in chars.chunks(size) {
            out.push_str(&filter.push(&piece.iter().collect::<String>()));
        }
        out.push_str(&filter.finish());
        out
    }

    #[test]
    fn plain_text_is_never_changed_whatever_the_chunking() {
        let text =
            "Intro line\n    indented ``` inline\n```rust\nfn main() {}\n```\nlast without newline";
        for size in [1, 2, 3, 7, 1000] {
            assert_eq!(run(text, size), text, "chunks of {size}");
        }
    }

    #[test]
    fn a_valid_block_is_removed() {
        let text = format!("Before\n{BLOCK}After\n");
        for size in [1, 4, 11, 1000] {
            assert_eq!(run(&text, size), "Before\nAfter\n", "chunks of {size}");
        }
    }

    #[test]
    fn two_blocks_in_one_answer() {
        let text = format!("{BLOCK}middle\n{BLOCK}");
        assert_eq!(run(&text, 5), "middle\n");
    }

    #[test]
    fn an_invalid_block_stays_as_text() {
        let text = "Look:\n```clusia-suggestion\n{not json}\n```\nDone\n";
        for size in [1, 6, 1000] {
            assert_eq!(run(text, size), text, "chunks of {size}");
        }
    }

    #[test]
    fn an_unclosed_block_is_released_at_the_end() {
        let text = "Look:\n```clusia-suggestion\n{\"file\":\"a.rs\"";
        assert_eq!(run(text, 3), text);
    }
}

//! The fenced blocks an agent writes into its answer: one scanner for every block kind.

/// The fence that opens and closes a block.
pub(crate) const FENCE: &str = "```";

/// The answer without its blocks, and the blocks in order as (info string, body).
///
/// A block opens on a line that, trimmed, is exactly the fence followed by one of `infos`, and
/// closes on the next line that, trimmed, is exactly the fence; its body is the lines between.
/// A block that never closes stays in the text and is not returned. The blank line that set a
/// block apart goes with it.
pub(crate) fn fenced_blocks<'a>(text: &'a str, infos: &[&str]) -> (String, Vec<(&'a str, String)>) {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(info) = lines[i]
            .trim()
            .strip_prefix(FENCE)
            .filter(|info| infos.contains(info))
            && let Some(close) = lines[i + 1..].iter().position(|l| l.trim() == FENCE)
        {
            blocks.push((info, lines[i + 1..i + 1 + close].join("\n")));
            i += close + 2;
            if lines.get(i).is_some_and(|l| l.trim().is_empty())
                && kept.last().is_none_or(|l| l.trim().is_empty())
            {
                i += 1;
            }
            continue;
        }
        kept.push(lines[i]);
        i += 1;
    }
    (kept.join("\n").trim_end().to_string(), blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_leave_the_text_in_order_with_their_info() {
        let text = "Intro\n\n```a\n{\"x\":1}\n```\n\nMiddle\n```b\nline1\nline2\n```\nEnd";
        let (prose, blocks) = fenced_blocks(text, &["a", "b"]);
        assert_eq!(prose, "Intro\n\nMiddle\nEnd");
        assert_eq!(
            blocks,
            vec![
                ("a", "{\"x\":1}".to_string()),
                ("b", "line1\nline2".to_string())
            ]
        );
    }

    #[test]
    fn an_info_must_match_the_whole_line() {
        let text = "```a-plan\nx\n```\n```ab\ny\n```\n```a\nz\n```";
        let (prose, blocks) = fenced_blocks(text, &["a"]);
        assert_eq!(blocks, vec![("a", "z".to_string())]);
        assert!(prose.contains("```a-plan") && prose.contains("```ab"));
    }

    #[test]
    fn a_block_that_never_closes_stays_in_the_text() {
        let (prose, blocks) = fenced_blocks("Before\n```a\n{}", &["a"]);
        assert!(blocks.is_empty());
        assert_eq!(prose, "Before\n```a\n{}");
    }

    #[test]
    fn indented_fences_count_and_other_fences_are_left_alone() {
        let text = "  ```a\n  {}\n  ```\n```rust\nfn main() {}\n```";
        let (prose, blocks) = fenced_blocks(text, &["a"]);
        assert_eq!(blocks, vec![("a", "  {}".to_string())]);
        assert_eq!(prose, "```rust\nfn main() {}\n```");
    }
}

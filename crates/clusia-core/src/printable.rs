//! Text from the agent or a command, made safe to show: what draws nothing or reorders what is
//! drawn would let a command disguise what the reviewer reads before allowing it.

/// Characters that draw nothing or reorder what is drawn: Unicode categories Cf, Zl and Zp
/// (bidi controls, zero-width characters, the BOM, tags, line and paragraph separators).
/// `char` has no category lookup, so the Cf ranges are listed.
pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{AD}' | '\u{600}'..='\u{605}' | '\u{61C}' | '\u{6DD}' | '\u{70F}'
        | '\u{890}'..='\u{891}' | '\u{8E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
        | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}' | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E007F}')
}

/// `text` as one safe line: a control character (a line break, a carriage return, an escape
/// sequence) or an invisible one (a bidi override, a zero-width character) is shown as an escape.
pub fn printable(text: &str) -> String {
    escaped(text, |_| false)
}

/// `printable`, keeping line breaks and tabs: for an excerpt shown as a block (an edit's old
/// and new text).
pub fn printable_lines(text: &str) -> String {
    escaped(text, |c| c == '\n' || c == '\t')
}

fn escaped(text: &str, keep: impl Fn(char) -> bool) -> String {
    text.chars()
        .map(|c| {
            if !keep(c) && (c.is_control() || is_invisible(c)) {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_invisible_characters_are_shown_as_escapes() {
        assert_eq!(printable("echo ok #\u{202e}x"), "echo ok #\\u{202e}x");
        assert_eq!(printable("a\rb\u{1b}[2K"), "a\\rb\\u{1b}[2K");
        assert_eq!(
            printable("ls\u{200b}-la\u{feff}"),
            "ls\\u{200b}-la\\u{feff}"
        );
        assert_eq!(printable("a\u{2028}b\u{2029}c"), "a\\u{2028}b\\u{2029}c");
        assert_eq!(printable("one\ntwo"), "one\\ntwo", "one line");
        assert_eq!(printable("naïve ✓ 日本"), "naïve ✓ 日本");
    }

    #[test]
    fn printable_lines_keeps_line_breaks_and_tabs_only() {
        assert_eq!(printable_lines("old\n→\n\tnew"), "old\n→\n\tnew");
        assert_eq!(printable_lines("a\r\nb"), "a\\r\nb");
        assert_eq!(
            printable_lines("x\u{2028}y\u{202e}"),
            "x\\u{2028}y\\u{202e}"
        );
    }

    #[test]
    fn invisible_characters_are_known() {
        for c in [
            '\u{AD}',
            '\u{200B}',
            '\u{202E}',
            '\u{2066}',
            '\u{FEFF}',
            '\u{E0041}',
        ] {
            assert!(is_invisible(c), "{c:?}");
        }
        for c in ['a', ' ', '→', '✓'] {
            assert!(!is_invisible(c), "{c:?}");
        }
    }
}

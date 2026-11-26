//! Cleaning and bounding of text that reaches the agent.
//!
//! Build output, machine names and controller messages are data from outside
//! the bridge. Terminal escape sequences and control characters are removed
//! before any of it is shown, and every string is bounded.

/// Remove ANSI/ECMA-48 escape sequences and every control character except
/// newline and tab. Bidirectional override characters are removed as well,
/// because they change how surrounding text reads without being visible.
pub(crate) fn strip_controls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            // 8-bit CSI.
            '\u{9b}' => skip_csi(&mut chars),
            // 8-bit DCS, SOS, OSC, PM, APC: strings that run to a terminator.
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_string(&mut chars),
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c if is_bidi_control(c) => {}
            c => out.push(c),
        }
    }
    out
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Consume what follows an ESC that has already been read.
fn skip_escape(chars: &mut Chars<'_>) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            skip_csi(chars);
        }
        // OSC, DCS, SOS, PM, APC.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            chars.next();
            skip_string(chars);
        }
        Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
            // nF sequence: intermediates then one final byte.
            while chars.next_if(|c| ('\u{20}'..='\u{2f}').contains(c)).is_some() {}
            chars.next_if(|c| ('\u{30}'..='\u{7e}').contains(c));
        }
        Some(c) if ('\u{30}'..='\u{7e}').contains(&c) => {
            chars.next();
        }
        // A lone ESC, or ESC before a control character or newline: drop
        // only the ESC and let the main loop judge what follows.
        _ => {}
    }
}

/// Parameters `0x30..=0x3f`, intermediates `0x20..=0x2f`, one final byte.
fn skip_csi(chars: &mut Chars<'_>) {
    while chars.next_if(|c| ('\u{30}'..='\u{3f}').contains(c)).is_some() {}
    while chars.next_if(|c| ('\u{20}'..='\u{2f}').contains(c)).is_some() {}
    chars.next_if(|c| ('\u{40}'..='\u{7e}').contains(c));
}

/// A control string ends at BEL, at ST (`ESC \` or `0x9c`), or at a line
/// break. Stopping at the line break keeps an unterminated sequence from
/// swallowing the rest of a log.
fn skip_string(chars: &mut Chars<'_>) {
    while let Some(c) = chars.peek().copied() {
        match c {
            '\n' => return,
            '\u{07}' | '\u{9c}' => {
                chars.next();
                return;
            }
            '\u{1b}' => {
                chars.next();
                chars.next_if_eq(&'\\');
                return;
            }
            _ => {
                chars.next();
            }
        }
    }
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
}

/// One line of display text: controls removed, whitespace runs (including
/// line breaks) collapsed to single spaces, at most `max_chars` characters.
pub(crate) fn inline(input: &str, max_chars: usize) -> String {
    let cleaned = strip_controls(input);
    let mut out = String::with_capacity(cleaned.len().min(max_chars * 4));
    let mut count = 0usize;
    let mut pending_space = false;
    for c in cleaned.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        let needed = 1 + usize::from(pending_space);
        if count + needed > max_chars {
            // Leave room for the ellipsis inside the bound.
            while count + 1 > max_chars {
                if out.pop().is_none() {
                    break;
                }
                count -= 1;
            }
            let trimmed = out.trim_end().len();
            out.truncate(trimmed);
            if max_chars > 0 {
                out.push('…');
            }
            return out;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
        count += needed;
    }
    out
}

/// The longest prefix of `s` that is at most `max_bytes` long and ends on a
/// character boundary.
pub(crate) fn prefix_within(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The longest suffix of `s` that is at most `max_bytes` long and starts on
/// a character boundary.
pub(crate) fn suffix_within(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_codes_and_cursor_movement_are_removed() {
        let raw = "\u{1b}[1;31merror\u{1b}[0m: \u{1b}[2K\u{1b}[1Gdone\u{1b}[?25h";
        assert_eq!(strip_controls(raw), "error: done");
    }

    #[test]
    fn operating_system_commands_are_removed_whole() {
        // A hyperlink, a window title ended by BEL and one ended by ST.
        let raw = "a\u{1b}]8;;https://example.invalid\u{1b}\\link\u{1b}]8;;\u{1b}\\b\u{1b}]0;title\u{7}c";
        assert_eq!(strip_controls(raw), "alinkbc");
    }

    #[test]
    fn newline_and_tab_survive_other_controls_do_not() {
        let raw = "one\r\n\ttwo\u{0}\u{8}\u{7f}\u{85}three";
        assert_eq!(strip_controls(raw), "one\n\ttwothree");
    }

    #[test]
    fn eight_bit_sequences_are_removed() {
        assert_eq!(strip_controls("x\u{9b}31my"), "xy");
        assert_eq!(strip_controls("x\u{9d}0;t\u{9c}y"), "xy");
    }

    #[test]
    fn an_unterminated_string_stops_at_the_line_break() {
        assert_eq!(strip_controls("a\u{1b}]0;never closed\nnext"), "a\nnext");
    }

    #[test]
    fn a_trailing_escape_is_dropped() {
        assert_eq!(strip_controls("tail\u{1b}"), "tail");
        assert_eq!(strip_controls("tail\u{1b}["), "tail");
        assert_eq!(strip_controls("a\u{1b}\nb"), "a\nb");
    }

    #[test]
    fn bidi_overrides_are_removed() {
        assert_eq!(strip_controls("safe\u{202e}txt.exe"), "safetxt.exe");
    }

    #[test]
    fn inline_collapses_and_bounds() {
        assert_eq!(inline("  a\n\n b\t c  ", 80), "a b c");
        assert_eq!(inline("abcdefgh", 5), "abcd…");
        assert_eq!(inline("abc", 3), "abc");
        assert_eq!(inline("ab cd", 3), "ab…");
        assert_eq!(inline("\u{1b}[31mred\u{1b}[0m", 10), "red");
        assert!(inline("ünïcödé text here", 6).chars().count() <= 6);
        assert_eq!(inline("anything", 0), "");
    }

    #[test]
    fn byte_bounds_respect_character_boundaries() {
        assert_eq!(prefix_within("héllo", 2), "h");
        assert_eq!(prefix_within("héllo", 3), "hé");
        assert_eq!(suffix_within("héllo", 4), "llo");
        assert_eq!(suffix_within("héllo", 5), "éllo");
        assert_eq!(prefix_within("abc", 10), "abc");
    }
}

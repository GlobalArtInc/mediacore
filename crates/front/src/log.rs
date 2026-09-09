//! Text this process did not write, on its way into a log line.
//!
//! A `tracing` fmt layer writes a field's `Display` into the line exactly as it
//! comes, so a value carrying a newline does not write a long line -- it writes
//! **two**, and the second one is whoever supplied the value speaking in this
//! process's voice. An unauthenticated visitor needed nothing but their own
//! round trip for it: `GET /` for a state, then
//! `/auth/callback?state=<theirs>&error=%0a…`, and the log grew a convincing
//! `signed somebody in subject=admin`. Somebody reading that log afterwards has
//! no way to tell it from a line this binary wrote.
//!
//! So everything from outside -- a query parameter, a header, a claim, an
//! error carrying a string a provider chose -- goes through [`quoted`] on its
//! way to a field. It is one function rather than a rule to remember at each
//! call site, and the rule it enforces is: **one value, one line, bounded.**

use std::fmt;

/// How much of somebody else's text a line will carry. Long enough for a
/// provider's error code, a `kid` or a URL; short enough that a megabyte of
/// `?error=` costs the log a line rather than a megabyte -- the query string is
/// the visitor's to make as long as a request will hold.
pub const MAX_LOGGED_CHARS: usize = 200;

/// The Unicode characters that are not `char::is_control` and still move text
/// about on a screen: the two separators a viewer may break a line at, and the
/// bidirectional overrides that let the rest of a line be *displayed* in an
/// order it is not written in. The same trick as CVE-2021-42574 in source
/// code, and a log is read by the same kind of eye.
fn rearranges_a_line(character: char) -> bool {
    matches!(
        character,
        '\u{2028}' | '\u{2029}' | '\u{061c}' | '\u{200e}' | '\u{200f}'
    ) || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// One log field, from text this process did not write.
///
/// The value is rendered between quotes so the reader can see where somebody
/// else's words start and stop; a newline, a tab, a control character and
/// anything that would reorder the line become escapes; and it is cut at
/// [`MAX_LOGGED_CHARS`], with a `…` after the closing quote so a cut value is
/// never mistaken for a whole one.
///
/// Printable text outside ASCII is left alone. A display name is `Роман` far
/// more often than it is an attack, and a log nobody can read is its own kind
/// of failure.
pub fn quoted(value: impl fmt::Display) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len().min(MAX_LOGGED_CHARS * 2) + 3);

    out.push('"');
    // One character in, one or more out: the count is of what the value
    // brought, so an escape that renders as ten characters still costs the
    // value one of its two hundred.
    for (written, character) in raw.chars().enumerate() {
        if written == MAX_LOGGED_CHARS {
            out.push('"');
            out.push('…');

            return out;
        }

        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other.is_control() || rearranges_a_line(other) => {
                out.push_str(&format!("\\u{{{:04x}}}", other as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');

    out
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod tests;

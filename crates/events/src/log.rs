//! Text this process did not write, on its way into a log line.
//!
//! The lines this crate writes when a delivery is abandoned carry three things
//! that came from outside it: the room's name and a participant's identity,
//! which are whatever a customer's backend put in a token, and the endpoint,
//! which is whatever an operator put in the config. A `tracing` fmt layer
//! writes a field's `Display` into the line exactly as it comes, so a value
//! carrying a newline does not write a long line -- it writes **two**, and the
//! second one is whoever supplied the value speaking in this process's voice.
//! An abandoned webhook is precisely the line somebody reads at three in the
//! morning to work out what went missing, so it is the last line that should
//! be forgeable.
//!
//! The rule is one value, one line, bounded, and it is the same rule and the
//! same shape as `mediacore_front::log`. It is written twice rather than
//! shared because the front end is a separate binary with no reason to link
//! this crate and this crate has no reason to link an OIDC client; the day a
//! third crate needs it, the three of them want one small crate of their own
//! rather than a dependency edge between two ends of the product.

use std::fmt;

/// How much of somebody else's text a line will carry. Long enough for a room
/// name, an identity or a URL; short enough that a room named with a megabyte
/// of newlines costs the log a line rather than a megabyte.
pub const MAX_LOGGED_CHARS: usize = 200;

/// The characters that are not `char::is_control` and still move text about on
/// a screen: the two separators a viewer may break a line at, and the
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
/// never mistaken for a whole one. Printable text outside ASCII is left alone:
/// a room is called `планёрка` far more often than it is an attack.
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

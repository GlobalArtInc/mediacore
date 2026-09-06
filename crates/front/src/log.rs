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
mod tests {
    use super::*;

    /// The forgery the review produced, in one assertion: whatever a visitor
    /// sends, what reaches the log is one line.
    #[test]
    fn nothing_a_visitor_sends_can_become_a_line_of_its_own() {
        let forged = "access_denied\n1999-01-01T00:00:00Z  INFO mediacore_front::auth: signed \
                      somebody in subject=admin roles=9";
        let rendered = quoted(forged);

        assert_eq!(
            rendered.lines().count(),
            1,
            "a query parameter wrote a second log line, and it reads as this process's own: \
             {rendered}"
        );
        assert!(
            rendered.contains("\\n1999-01-01"),
            "the newline was not escaped, only removed, so the line it forged is still there: \
             {rendered}"
        );
        assert!(
            rendered.contains("access_denied"),
            "the reason the provider gave stopped being readable: {rendered}"
        );
    }

    /// `\n` is not the only thing that starts a line. A terminal, a `less` and
    /// a log viewer between them break at a carriage return, at the C1 next
    /// line, and at the Unicode line separator.
    #[test]
    fn every_way_of_starting_a_new_line_is_escaped() {
        for (raw, escape) in [
            ("a\rb", "\\r"),
            ("a\u{b}b", "\\u{000b}"),
            ("a\u{c}b", "\\u{000c}"),
            ("a\u{85}b", "\\u{0085}"),
            ("a\u{2028}b", "\\u{2028}"),
            ("a\u{2029}b", "\\u{2029}"),
        ] {
            let rendered = quoted(raw);

            assert!(
                rendered.contains(escape) && rendered.chars().all(|c| !c.is_control()),
                "`{raw:?}` reached the log as it was written, and it starts a line on somebody's \
                 screen: {rendered}"
            );
        }
    }

    /// A line that can be *displayed* in an order it was not written in is a
    /// line that can say the opposite of what it holds.
    #[test]
    fn nothing_can_rearrange_the_line_it_is_written_into() {
        let rendered = quoted("access_denied\u{202e}nimda=tcejbus ni ydobemos dengis");

        assert!(
            !rendered.contains('\u{202e}'),
            "a right-to-left override survived, so the rest of the line reads backwards on a \
             screen: {rendered}"
        );
        assert!(rendered.contains("\\u{202e}"), "{rendered}");
    }

    #[test]
    fn a_megabyte_of_somebody_elses_text_costs_the_log_a_line() {
        let rendered = quoted("a".repeat(1024 * 1024));

        assert!(
            rendered.chars().count() <= MAX_LOGGED_CHARS + 3,
            "a log line carried {} characters of a visitor's choosing",
            rendered.chars().count()
        );
        assert!(
            rendered.ends_with('…'),
            "a cut value is not marked as cut, so the log reads as if that was all of it: \
             {rendered}"
        );
    }

    #[test]
    fn what_a_person_is_called_survives_being_logged() {
        assert_eq!(quoted("Роман"), "\"Роман\"");
        assert_eq!(quoted("access_denied"), "\"access_denied\"");
        assert_eq!(quoted(""), "\"\"");
    }

    /// The quotes are the reader's fence, so a value carrying one of its own
    /// must not be able to close it early and write what looks like another
    /// field beside it.
    #[test]
    fn a_value_cannot_close_the_quotes_it_is_written_between() {
        let rendered = quoted(r#"a" subject="admin"#);

        assert_eq!(
            rendered, r#""a\" subject=\"admin""#,
            "a quote in the value ended the field early: {rendered}"
        );
    }
}

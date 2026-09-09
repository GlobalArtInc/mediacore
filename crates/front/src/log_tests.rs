
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

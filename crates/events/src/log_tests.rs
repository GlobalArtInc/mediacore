use super::*;

/// A room name reaches this server from a token somebody else minted. The
/// line that says a delivery was abandoned is the one an operator trusts
/// to tell them what went missing, so a room name must not be able to
/// write a line of its own beside it.
#[test]
fn a_room_name_cannot_write_a_line_of_its_own() {
    let forged = "standup\n1999-01-01T00:00:00Z  INFO mediacore_events: delivered event \
                      id=EV_000000000000 status=200";
    let rendered = quoted(forged);

    assert_eq!(
        rendered.lines().count(),
        1,
        "a room name wrote a second log line, and it reads as this process's own: {rendered}"
    );
    assert!(
        rendered.contains("\\n1999-01-01"),
        "the newline was removed rather than escaped, so the line it forged is still there: \
             {rendered}"
    );
}

#[test]
fn every_way_of_starting_or_reordering_a_line_is_escaped() {
    for (raw, escape) in [
        ("a\rb", "\\r"),
        ("a\u{85}b", "\\u{0085}"),
        ("a\u{2028}b", "\\u{2028}"),
        ("a\u{202e}b", "\\u{202e}"),
    ] {
        let rendered = quoted(raw);

        assert!(
            rendered.contains(escape),
            "`{raw:?}` reached the log as it was written, and it starts or reverses a line on \
                 somebody's screen: {rendered}"
        );
    }
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
fn what_a_room_is_called_survives_being_logged() {
    assert_eq!(quoted("планёрка"), "\"планёрка\"");
    assert_eq!(
        quoted("https://example.test/hook"),
        "\"https://example.test/hook\""
    );
}

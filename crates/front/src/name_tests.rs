
use super::*;

#[test]
fn trims_and_keeps_the_typed_name() {
    assert_eq!(clean_name("  Alice  ").unwrap(), "Alice");
    assert_eq!(clean_name("Роман").unwrap(), "Роман");
}

#[test]
fn refuses_an_empty_name() {
    assert_eq!(clean_name(""), Err(NameError::Empty));
    assert_eq!(clean_name("   \t "), Err(NameError::Empty));
}

#[test]
fn refuses_an_oversized_name() {
    let long = "a".repeat(MAX_NAME_CHARS + 1);
    assert_eq!(clean_name(&long), Err(NameError::TooLong));
    assert!(clean_name(&"a".repeat(MAX_NAME_CHARS)).is_ok());
}

#[test]
fn counts_characters_not_bytes() {
    let cyrillic = "я".repeat(MAX_NAME_CHARS);
    assert!(cyrillic.len() > MAX_NAME_CHARS);
    assert!(clean_name(&cyrillic).is_ok());
}

#[test]
fn refuses_control_characters() {
    assert_eq!(clean_name("ali\u{7}ce"), Err(NameError::Control));
}

#[test]
fn two_identical_names_get_distinct_identities() {
    let first = identity_for("Alice");
    let second = identity_for("Alice");

    assert_ne!(first, second);
    assert!(first.starts_with("alice-"));
    assert!(second.starts_with("alice-"));
}

#[test]
fn identity_is_ascii_and_bounded() {
    let long = "a".repeat(80);
    for name in ["Роман", "Ann-Marie O'Neill", "!!!", long.as_str()] {
        let identity = identity_for(name);
        assert!(identity.is_ascii(), "{identity} is not ascii");
        assert!(identity.len() <= MAX_SLUG_CHARS + 1 + SUFFIX_CHARS);
        assert!(!identity.starts_with('-'));
    }
}

#[test]
fn a_name_without_ascii_letters_falls_back() {
    assert!(identity_for("Роман").starts_with("guest-"));
}

#[test]
fn a_requested_room_stays_inside_the_namespace() {
    assert_eq!(room_for("demo", "standup").unwrap(), "demo.standup");
    assert_eq!(room_for("demo", " standup ").unwrap(), "demo.standup");

    for requested in ["ops", "board", "a.b", "..", "-", "_", "Demo", "demo2"] {
        let room = room_for("demo", requested).unwrap();
        assert!(
            room.starts_with("demo."),
            "`{requested}` escaped the namespace as `{room}`"
        );
        assert_ne!(room, "demo");
        assert_ne!(room, requested);
    }
}

/// The namespace is a room, and it is the room the bare page has always
/// joined. Naming it has to reach it, or `/r/demo` and `/` would be two
/// different rooms and a reload would move whoever pressed it.
#[test]
fn naming_the_namespace_reaches_the_room_everyone_lands_in() {
    assert_eq!(room_for("demo", "demo").unwrap(), "demo");
    assert_eq!(room_for("demo", " demo ").unwrap(), "demo");
    assert_eq!(
        room_for("rooms.mediacore", "rooms.mediacore").unwrap(),
        "rooms.mediacore"
    );
}

/// The whole set a caller can reach, stated as the property rather than as
/// a composition: this room, or one under it, and nothing else.
#[test]
fn nothing_a_caller_can_ask_for_reaches_another_namespace() {
    for requested in ["demo", "ops", "..", "-", "demo.ops", "d", "demo-other"] {
        let room = room_for("demo", requested).unwrap();
        assert!(
            room == "demo" || room.starts_with("demo."),
            "`{requested}` reached `{room}`, which is a room this front end does not own"
        );
    }
}

#[test]
fn a_room_that_cannot_be_namespaced_is_refused() {
    assert_eq!(room_for("demo", ""), Err(RoomError::Empty));
    assert_eq!(room_for("demo", "not a room"), Err(RoomError::Charset));
    assert_eq!(room_for("demo", "../other"), Err(RoomError::Charset));
    assert_eq!(
        room_for("demo", &"a".repeat(MAX_ROOM_CHARS)),
        Err(RoomError::TooLong)
    );
}

#[test]
fn room_names_are_checked() {
    assert_eq!(clean_room(" demo ").unwrap(), "demo");
    assert_eq!(clean_room(""), Err(RoomError::Empty));
    assert_eq!(clean_room("a b"), Err(RoomError::Charset));
    assert_eq!(
        clean_room(&"a".repeat(MAX_ROOM_CHARS + 1)),
        Err(RoomError::TooLong)
    );
}

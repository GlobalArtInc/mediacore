use uuid::Uuid;

pub const MAX_NAME_CHARS: usize = 40;
pub const MAX_ROOM_CHARS: usize = 64;
pub const ROOM_NAMESPACE_SEPARATOR: char = '.';
const MAX_SLUG_CHARS: usize = 24;
const SUFFIX_CHARS: usize = 8;
const FALLBACK_SLUG: &str = "guest";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("a name is required")]
    Empty,
    #[error("a name may be at most {MAX_NAME_CHARS} characters long")]
    TooLong,
    #[error("a name may not contain control characters")]
    Control,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoomError {
    #[error("a room name is required")]
    Empty,
    #[error("a room name may be at most {MAX_ROOM_CHARS} characters long")]
    TooLong,
    #[error("a room name may only contain letters, digits, `-`, `_` and `.`")]
    Charset,
}

pub fn clean_name(raw: &str) -> Result<String, NameError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(NameError::TooLong);
    }
    if name.chars().any(char::is_control) {
        return Err(NameError::Control);
    }
    Ok(name.to_owned())
}

pub fn clean_room(raw: &str) -> Result<String, RoomError> {
    let room = raw.trim();
    if room.is_empty() {
        return Err(RoomError::Empty);
    }
    if room.chars().count() > MAX_ROOM_CHARS {
        return Err(RoomError::TooLong);
    }
    if !room
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(RoomError::Charset);
    }
    Ok(room.to_owned())
}

/// The room a caller asking for `requested` actually reaches, given the
/// namespace this front end owns.
///
/// Two answers, and only two: the namespace itself, or a name inside it. That
/// is the same set of rooms this endpoint could reach when the room was a
/// server-wide setting and no request could name one at all -- `demo`, from a
/// body with no `room` field -- plus the `demo.` names it has minted since. A
/// caller who sends `board`, `..` or `ops.private` lands in `demo.board`,
/// `demo...` and `demo.ops.private`; `.` and `-` are the only punctuation the
/// charset allows and neither can walk up a namespace.
///
/// The namespace resolving to *itself* rather than to `demo.demo` is not a
/// convenience. `/` and `/r/demo` name the room everyone lands in, and a
/// reload has to come back to the room the first load joined; composing there
/// would put the second one in `demo.demo` and quietly split the room in two.
pub fn room_for(namespace: &str, requested: &str) -> Result<String, RoomError> {
    let requested = clean_room(requested)?;
    if requested == namespace {
        return Ok(requested);
    }

    let room = format!("{namespace}{ROOM_NAMESPACE_SEPARATOR}{requested}");
    if room.chars().count() > MAX_ROOM_CHARS {
        return Err(RoomError::TooLong);
    }
    Ok(room)
}

pub fn identity_for(name: &str) -> String {
    format!("{}-{}", slug(name), suffix())
}

fn slug(name: &str) -> String {
    let mut slug = String::with_capacity(MAX_SLUG_CHARS);
    let mut pending_dash = false;

    for c in name.chars() {
        if slug.chars().count() >= MAX_SLUG_CHARS {
            break;
        }
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }

    if slug.is_empty() {
        FALLBACK_SLUG.to_owned()
    } else {
        slug
    }
}

fn suffix() -> String {
    Uuid::new_v4().simple().to_string()[..SUFFIX_CHARS].to_owned()
}

#[cfg(test)]
mod tests {
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
}

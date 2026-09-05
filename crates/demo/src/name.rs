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

pub fn namespaced_room(namespace: &str, requested: &str) -> Result<String, RoomError> {
    let requested = clean_room(requested)?;
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
        assert_eq!(namespaced_room("demo", "standup").unwrap(), "demo.standup");
        assert_eq!(
            namespaced_room("demo", " standup ").unwrap(),
            "demo.standup"
        );
        assert_eq!(namespaced_room("demo", "demo").unwrap(), "demo.demo");

        for requested in ["ops", "demo", "a.b", "..", "-", "_"] {
            let room = namespaced_room("demo", requested).unwrap();
            assert!(
                room.starts_with("demo."),
                "`{requested}` escaped the namespace as `{room}`"
            );
            assert_ne!(room, "demo");
            assert_ne!(room, requested);
        }
    }

    #[test]
    fn a_room_that_cannot_be_namespaced_is_refused() {
        assert_eq!(namespaced_room("demo", ""), Err(RoomError::Empty));
        assert_eq!(
            namespaced_room("demo", "not a room"),
            Err(RoomError::Charset)
        );
        assert_eq!(namespaced_room("demo", "../other"), Err(RoomError::Charset));
        assert_eq!(
            namespaced_room("demo", &"a".repeat(MAX_ROOM_CHARS)),
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

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
#[path = "name_tests.rs"]
mod tests;

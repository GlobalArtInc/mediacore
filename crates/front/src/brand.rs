//! Whose product this is, as the page says it.
//!
//! A deployment that has bought this server puts its own name and its own logo
//! on it, and the three places a visitor reads one -- the tab, the lobby card
//! and the heading over the roster -- have to agree. So there is one brand, it
//! comes from the configuration, and it is rendered into the page once at
//! startup rather than painted by a script afterwards. That last part is the
//! whole point: `/api/config` lands after the first paint, so a brand painted
//! from it would show our wordmark for one round trip on every load, on the one
//! screen whose entire job is to say whose product this is. It would also leave
//! the tab wrong, because a tab's name is markup and is what a bookmark, a
//! history entry and a link preview all record.
//!
//! Off is not a degraded mode. With nothing configured the anchors already
//! spell the default, `render` hands back the bytes it was given, and the
//! server ships exactly the page it shipped before any of this existed.

use std::borrow::Cow;
use std::fmt;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

pub const DEFAULT_NAME: &str = "MediaCore";

/// Long enough for a company, short enough to stay on one line in a card that
/// is 26rem wide and in a heading that ellipsises rather than wraps.
pub const MAX_NAME_CHARS: usize = 64;

/// A logo is a wordmark in a header, not an illustration. A quarter of a
/// megabyte is generous for one, and it is also about where an environment
/// variable stops being a reasonable place to keep something.
pub const MAX_LOGO_BYTES: usize = 256 * 1024;

const LOGO_PREFIX: &str = "/brand/logo-";

/// Enough of the digest to name the bytes. This is a cache key, not a
/// signature: it exists so a changed logo is a changed address.
const LOGO_STAMP_BYTES: usize = 8;
const DATA_URI_PREFIX: &str = "data:";

/// The three strings in `assets/index.html` this module rewrites.
///
/// They are matched literally rather than parsed, and each one is asserted to
/// occur exactly once, so an edit to the page that moves a brand out from
/// behind one of them fails a test here instead of quietly shipping our name on
/// somebody else's install.
const TITLE_ANCHOR: &str = "<title>MediaCore</title>";
const LOBBY_ANCHOR: &str = r#"<span class="brand-mark">MediaCore</span>"#;
const ROSTER_ANCHOR: &str = r#"<h1 class="brand-mark room-brand">MediaCore</h1>"#;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrandError {
    #[error("a brand name is at most {MAX_NAME_CHARS} characters, and this one is {count}")]
    NameTooLong { count: usize },
    #[error("a brand name goes in the tab and in an alt attribute, so it cannot carry control characters")]
    NameControl,
    #[error("{path}: {reason}")]
    LogoUnreadable { path: String, reason: String },
    #[error("a logo is at most {} KiB, and this one is {} KiB", MAX_LOGO_BYTES / 1024, .size / 1024)]
    LogoTooLarge { size: usize },
    #[error("the logo is not valid base64: {0}")]
    LogoNotBase64(String),
    #[error(
        "the logo is not a png, jpeg, gif, webp or svg. Its type is read from the bytes \
         themselves rather than from a file name, so a renamed file is still refused"
    )]
    LogoUnrecognised,
    #[error("a logo can be given inline or as a path, and this deployment gave both")]
    LogoGivenTwice,
}

/// The name, and the picture that stands in for it where there is room for one.
///
/// The name is not optional and does not go away when a logo is set. A tab
/// cannot show a picture, a screen reader cannot read one, and `alt` has to say
/// something -- so the logo is only ever what a header draws *instead of* the
/// words, never a reason not to have any.
#[derive(Clone, PartialEq, Eq)]
pub struct Brand {
    pub name: String,
    pub logo: Option<Logo>,
}

/// An image this process read once and now serves itself.
///
/// The path carries a hash of the bytes, which is what lets the response be
/// cached for a year: a new logo is a new path, so no browser is ever left
/// holding the last customer's. Nothing about this reaches out at runtime, and
/// the content security policy this server sets is not touched -- `img-src
/// 'self'` already covers an image this binary serves from its own origin,
/// which is exactly why the logo is copied in here rather than linked to.
#[derive(Clone, PartialEq, Eq)]
pub struct Logo {
    pub path: String,
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

impl Default for Brand {
    fn default() -> Self {
        Self {
            name: DEFAULT_NAME.to_owned(),
            logo: None,
        }
    }
}

/// The bytes are a picture and the name might be a customer's, so neither is
/// printed. The shape is what a log line is for: whether this deployment is
/// branded at all.
impl fmt::Debug for Brand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Brand")
            .field("name", &self.name)
            .field("logo", &self.logo.as_ref().map(|logo| &logo.path))
            .finish()
    }
}

impl Brand {
    /// A brand from what an operator set, or the default when they set nothing.
    ///
    /// `inline` and `file` are the same two ways in that a licence key is
    /// offered, and for the same reason: a picture pasted into a Kubernetes
    /// Secret and a picture in a file on a VM are both things people do. Giving
    /// both is refused rather than resolved, because a server that silently
    /// picked one would put a logo nobody chose on the page.
    pub fn new(
        name: Option<&str>,
        inline: Option<&str>,
        file: Option<&str>,
    ) -> Result<Self, BrandError> {
        let name = match name {
            Some(name) => clean_name(name)?,
            None => DEFAULT_NAME.to_owned(),
        };

        let bytes = match (inline, file) {
            (Some(_), Some(_)) => return Err(BrandError::LogoGivenTwice),
            (Some(inline), None) => Some(decode(inline)?),
            (None, Some(path)) => Some(read(path)?),
            (None, None) => None,
        };

        let logo = bytes.map(Logo::new).transpose()?;

        Ok(Self { name, logo })
    }

    /// The page, with this brand in it.
    ///
    /// Borrowed and untouched when there is nothing to change, which is both
    /// the common case and the one where any difference at all would be a bug.
    pub fn render<'a>(&self, page: &'a str) -> Cow<'a, str> {
        if self.is_default() {
            return Cow::Borrowed(page);
        }

        let mark = self.mark();

        Cow::Owned(
            page.replace(TITLE_ANCHOR, &format!("<title>{}</title>", escape(&self.name)))
                .replace(
                    LOBBY_ANCHOR,
                    &format!(r#"<span class="brand-mark">{mark}</span>"#),
                )
                .replace(
                    ROSTER_ANCHOR,
                    &format!(r#"<h1 class="brand-mark room-brand">{mark}</h1>"#),
                ),
        )
    }

    /// What a header draws: the words, or the picture with the words behind it.
    pub fn mark(&self) -> String {
        let name = escape(&self.name);

        match &self.logo {
            Some(logo) => format!(
                r#"<img class="brand-logo" src="{}" alt="{name}" />"#,
                escape(&logo.path)
            ),
            None => name,
        }
    }

    fn is_default(&self) -> bool {
        self.name == DEFAULT_NAME && self.logo.is_none()
    }
}

impl fmt::Debug for Logo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logo")
            .field("path", &self.path)
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl Logo {
    fn new(bytes: Vec<u8>) -> Result<Self, BrandError> {
        if bytes.len() > MAX_LOGO_BYTES {
            return Err(BrandError::LogoTooLarge { size: bytes.len() });
        }

        let (content_type, extension) = image_type(&bytes)?;
        let digest = Sha256::digest(&bytes);
        let stamp: String = digest
            .iter()
            .take(LOGO_STAMP_BYTES)
            .map(|byte| format!("{byte:02x}"))
            .collect();

        Ok(Self {
            path: format!("{LOGO_PREFIX}{stamp}.{extension}"),
            content_type,
            bytes,
        })
    }
}

/// A brand name, or why this one cannot be one.
///
/// `&`, `<` and `"` are accepted rather than refused. `Bloom & Wild` is a real
/// company, and a server that argued with a customer's own name would be
/// wrong; keeping them out of the markup is this module's job, which `escape`
/// does at render time. Control characters are refused, because they carry no
/// name and would land in a `<title>` and in an attribute.
fn clean_name(name: &str) -> Result<String, BrandError> {
    let name = name.trim();
    let count = name.chars().count();

    if count > MAX_NAME_CHARS {
        return Err(BrandError::NameTooLong { count });
    }
    if name.chars().any(|character| character.is_control()) {
        return Err(BrandError::NameControl);
    }

    Ok(match name.is_empty() {
        true => DEFAULT_NAME.to_owned(),
        false => name.to_owned(),
    })
}

/// A logo out of an environment variable.
///
/// Both a bare base64 blob and a whole `data:` URI are accepted, because the
/// first is what `base64 -w0 logo.png` produces and the second is what a person
/// copies out of a browser or a design tool. Whitespace is dropped: a value
/// that travelled through a YAML block scalar arrives wrapped.
fn decode(value: &str) -> Result<Vec<u8>, BrandError> {
    let value = value.trim();
    let payload = match value.strip_prefix(DATA_URI_PREFIX) {
        Some(rest) => rest.split_once(',').map_or(rest, |(_, data)| data),
        None => value,
    };

    let payload: String = payload
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();

    STANDARD
        .decode(payload)
        .map_err(|error| BrandError::LogoNotBase64(error.to_string()))
}

/// The file, and never more of it than a logo could be.
///
/// The limit is enforced while reading rather than after, because `read` on a
/// path that turns out to be `/dev/zero` or a fifo does not come back: the
/// process would hang or be killed at startup with nothing in the log to say
/// why. One byte over the limit is enough for `Logo::new` to refuse it.
fn read(path: &str) -> Result<Vec<u8>, BrandError> {
    use std::io::Read as _;

    let refuse = |error: std::io::Error| BrandError::LogoUnreadable {
        path: path.to_owned(),
        reason: error.to_string(),
    };

    let file = std::fs::File::open(Path::new(path)).map_err(refuse)?;
    let mut bytes = Vec::new();
    file.take(MAX_LOGO_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(refuse)?;

    Ok(bytes)
}

/// What this actually is, from the bytes rather than from a file name.
///
/// A file name is what somebody typed and the magic number is what a browser
/// will believe, so reading the second is the only way a wrong `Content-Type`
/// becomes a refusal to start rather than a header that no longer matches the
/// body it is on.
fn image_type(bytes: &[u8]) -> Result<(&'static str, &'static str), BrandError> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const JPEG: &[u8] = &[0xff, 0xd8, 0xff];

    if bytes.starts_with(PNG) {
        return Ok(("image/png", "png"));
    }
    if bytes.starts_with(JPEG) {
        return Ok(("image/jpeg", "jpg"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok(("image/gif", "gif"));
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Ok(("image/webp", "webp"));
    }
    if is_svg(bytes) {
        return Ok(("image/svg+xml", "svg"));
    }

    Err(BrandError::LogoUnrecognised)
}

/// Whether this is an svg, worked out by reading rather than by guessing.
///
/// Svg is the only format here with no magic number, so the rule is written
/// out: skip a byte order mark, then skip any run of xml declarations,
/// comments and doctypes, and require what is left to open an `svg` element.
/// Lowercase only, because xml is case sensitive and every real export is
/// `<svg`. A doctype carrying an internal subset is refused rather than
/// resolved, which is the safe direction and something no ordinary export has.
///
/// Serving one is safe for a reason worth stating, because it is not obvious.
/// Inside an `<img>` a browser renders svg in a restricted mode with no
/// scripting and no external fetches at all. Reached directly it is a document
/// on this origin -- and this server's policy is `default-src 'none'` with
/// `script-src 'self'` and no `'unsafe-inline'`, so a script inside one has
/// nothing it is allowed to run.
fn is_svg(bytes: &[u8]) -> bool {
    const BOM: &[u8] = &[0xef, 0xbb, 0xbf];
    const HEAD: usize = 1024;

    let bytes = bytes.strip_prefix(BOM).unwrap_or(bytes);
    let mut head = match std::str::from_utf8(&bytes[..bytes.len().min(HEAD)]) {
        Ok(text) => text,
        Err(error) => match std::str::from_utf8(&bytes[..error.valid_up_to()]) {
            Ok(text) => text,
            Err(_) => return false,
        },
    };

    loop {
        head = head.trim_start();

        let skipped = if let Some(rest) = head.strip_prefix("<!--") {
            rest.split_once("-->")
        } else if let Some(rest) = head.strip_prefix("<?") {
            rest.split_once("?>")
        } else if let Some(rest) = head.strip_prefix("<!") {
            rest.split_once('>')
        } else {
            break;
        };

        match skipped {
            Some((_, rest)) => head = rest,
            None => return false,
        }
    }

    match head.strip_prefix("<svg") {
        Some(rest) => rest
            .chars()
            .next()
            .is_some_and(|next| next.is_whitespace() || next == '>' || next == '/'),
        None => false,
    }
}

/// The five entities, because a brand name is somebody else's text.
///
/// It lands in element content and in an `alt` attribute, so both quote forms
/// are covered rather than only the one the current markup happens to use.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(character),
        }
    }

    out
}

#[cfg(test)]
#[path = "brand_tests.rs"]
mod tests;

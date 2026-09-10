use super::*;

use crate::http::INDEX_HTML;

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01];
const JPEG: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
const GIF: &[u8] = b"GIF89a\x01\x00";
const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

fn logo(bytes: &[u8]) -> Brand {
    Brand::new(Some("Acme"), Some(&STANDARD.encode(bytes)), None).expect("a logo this shape loads")
}

/// The three strings this module rewrites are literals, so an edit to the page
/// that moves a brand out from behind one of them would silently ship our name
/// on somebody else's install. This is what stops that being silent.
#[test]
fn every_anchor_is_in_the_page_exactly_once() {
    for anchor in [TITLE_ANCHOR, LOBBY_ANCHOR, ROSTER_ANCHOR] {
        assert_eq!(
            INDEX_HTML.matches(anchor).count(),
            1,
            "`{anchor}` is not in assets/index.html exactly once, so a branded deployment \
             would keep our wordmark somewhere on the page"
        );
    }
}

#[test]
fn a_deployment_that_configured_nothing_gets_the_page_it_always_got() {
    let brand = Brand::new(None, None, None).unwrap();

    assert_eq!(brand.name, DEFAULT_NAME);
    assert!(brand.logo.is_none());
    assert!(
        matches!(brand.render(INDEX_HTML), Cow::Borrowed(_)),
        "the page was rebuilt for a deployment that asked for nothing"
    );
}

/// Setting the name to what it already is is not a configuration change, and a
/// server that rewrote the page for it would differ from one that left it unset
/// for no reason anybody could see.
#[test]
fn naming_the_default_is_the_same_as_naming_nothing() {
    let brand = Brand::new(Some("  MediaCore  "), None, None).unwrap();

    assert_eq!(brand.name, DEFAULT_NAME);
    assert!(matches!(brand.render(INDEX_HTML), Cow::Borrowed(_)));
}

#[test]
fn a_caption_reaches_the_tab_the_lobby_and_the_roster() {
    let brand = Brand::new(Some("Acme Rooms"), None, None).unwrap();
    let page = brand.render(INDEX_HTML);

    assert!(page.contains("<title>Acme Rooms</title>"));
    assert!(page.contains(r#"<span class="brand-mark">Acme Rooms</span>"#));
    assert!(page.contains(r#"<h1 class="brand-mark room-brand">Acme Rooms</h1>"#));
    assert!(
        !page.contains("MediaCore"),
        "our own name is still somewhere on a page that was branded for somebody else"
    );
}

/// `Bloom & Wild` is a real company, so the answer to punctuation is escaping
/// it, not arguing with a customer about their own name.
#[test]
fn a_name_with_punctuation_in_it_is_escaped_rather_than_refused() {
    let brand = Brand::new(Some(r#"Bloom & Wild "Rooms""#), None, None).unwrap();
    let page = brand.render(INDEX_HTML);

    assert!(page.contains("<title>Bloom &amp; Wild &quot;Rooms&quot;</title>"));
    assert!(!page.contains(r#""Rooms""#));
}

/// The page is written once and never scanned again, and the name is escaped
/// before it goes in -- so a name shaped like an anchor is text on the page and
/// not a second substitution.
#[test]
fn a_name_shaped_like_an_anchor_cannot_re_enter_the_substitution() {
    let hostile = r#"<h1 class="brand-mark room-brand">MediaCore</h1>"#;
    let brand = Brand::new(Some(hostile), None, None).unwrap();
    let page = brand.render(INDEX_HTML);

    assert!(
        !page.contains(hostile),
        "a brand name was put on the page as markup"
    );
    assert_eq!(
        page.matches(r#"<h1 class="brand-mark room-brand">"#).count(),
        1,
        "the roster heading was substituted more than once"
    );
    assert!(page.contains("&lt;h1 class=&quot;brand-mark room-brand&quot;&gt;"));
}

#[test]
fn a_logo_replaces_the_words_and_keeps_them_as_the_alt_text() {
    let brand = logo(PNG);
    let page = brand.render(INDEX_HTML);
    let path = &brand.logo.as_ref().unwrap().path;

    assert!(page.contains(&format!(r#"<img class="brand-logo" src="{path}" alt="Acme" />"#)));
    assert!(
        page.contains("<title>Acme</title>"),
        "a tab cannot show a picture, so it has to keep the words"
    );
}

/// The type comes from the bytes because a browser believes the header we send
/// and not the name of the file it came out of.
#[test]
fn the_type_of_a_logo_is_read_from_its_bytes() {
    for (bytes, content_type, extension) in [
        (PNG, "image/png", "png"),
        (JPEG, "image/jpeg", "jpg"),
        (GIF, "image/gif", "gif"),
        (WEBP, "image/webp", "webp"),
    ] {
        let brand = logo(bytes);
        let found = brand.logo.expect("this is a recognised image");

        assert_eq!(found.content_type, content_type);
        assert!(
            found.path.ends_with(&format!(".{extension}")),
            "{content_type} was served as {}",
            found.path
        );
    }
}

/// Svg has no magic number, so the rule has to be written out -- and the
/// wrappers a real export puts in front of the root element are exactly what a
/// naive `starts_with` gets wrong.
#[test]
fn an_svg_is_recognised_through_whatever_precedes_its_root() {
    let heads = [
        r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
        "<?xml version=\"1.0\"?>\n<svg></svg>",
        "<?xml version=\"1.0\"?>\n<!-- drawn by hand -->\n<svg/>",
        "<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"svg11.dtd\">\n<svg >",
        "\u{feff}<svg></svg>",
    ];

    for head in heads {
        let brand = logo(head.as_bytes());
        let found = brand.logo.expect("this is an svg");

        assert_eq!(found.content_type, "image/svg+xml", "{head}");
        assert!(found.path.ends_with(".svg"));
    }
}

#[test]
fn something_that_is_not_an_image_is_refused() {
    for bytes in [
        &b"not a picture at all"[..],
        &b"<html><body>hello</body></html>"[..],
        &b"<svgnot>"[..],
        &b"<!-- a comment that never ends <svg>"[..],
        &[][..],
    ] {
        assert_eq!(
            Brand::new(Some("Acme"), Some(&STANDARD.encode(bytes)), None).unwrap_err(),
            BrandError::LogoUnrecognised,
            "{:?} was taken for an image",
            String::from_utf8_lossy(&bytes[..bytes.len().min(24)])
        );
    }
}

#[test]
fn the_refusals_worth_making() {
    let name = "n".repeat(MAX_NAME_CHARS + 1);
    assert_eq!(
        Brand::new(Some(&name), None, None).unwrap_err(),
        BrandError::NameTooLong {
            count: MAX_NAME_CHARS + 1
        }
    );
    assert_eq!(
        Brand::new(Some(&"n".repeat(MAX_NAME_CHARS)), None, None)
            .unwrap()
            .name
            .chars()
            .count(),
        MAX_NAME_CHARS,
        "the limit itself was refused"
    );
    assert_eq!(
        Brand::new(Some("Acme\u{7}Rooms"), None, None).unwrap_err(),
        BrandError::NameControl
    );
    assert!(matches!(
        Brand::new(Some("Acme"), Some("not base64 at all !!"), None).unwrap_err(),
        BrandError::LogoNotBase64(_)
    ));
    assert_eq!(
        Brand::new(Some("Acme"), Some("AAAA"), Some("/logo.png")).unwrap_err(),
        BrandError::LogoGivenTwice,
        "a deployment that gave a logo two ways got one of them silently"
    );

    let big = STANDARD.encode(
        [PNG, &vec![0u8; MAX_LOGO_BYTES]]
            .concat()
            .as_slice(),
    );
    assert!(matches!(
        Brand::new(Some("Acme"), Some(&big), None).unwrap_err(),
        BrandError::LogoTooLarge { .. }
    ));
}

/// A file that is not there has to name itself, because the operator who typed
/// the path is the only person who can fix it.
#[test]
fn a_logo_file_that_is_not_there_names_itself() {
    let error = Brand::new(Some("Acme"), None, Some("/no/such/logo.png")).unwrap_err();

    assert!(matches!(
        error,
        BrandError::LogoUnreadable { ref path, .. } if path == "/no/such/logo.png"
    ));
    assert!(error.to_string().contains("/no/such/logo.png"));
}

/// A whole `data:` URI is what a person copies out of a design tool, and the
/// declared type in it is ignored: the bytes decide, so a mislabelled one is
/// served correctly rather than as whatever it claimed.
#[test]
fn a_data_uri_is_accepted_and_its_claimed_type_is_ignored() {
    let claimed = format!("data:image/gif;base64,{}", STANDARD.encode(PNG));
    let brand = Brand::new(Some("Acme"), Some(&claimed), None).unwrap();

    assert_eq!(brand.logo.unwrap().content_type, "image/png");
}

/// A value that travelled through a yaml block scalar arrives wrapped.
#[test]
fn base64_that_arrived_wrapped_over_several_lines_still_decodes() {
    let wrapped = STANDARD
        .encode([PNG, &[0u8; 200]].concat())
        .chars()
        .collect::<Vec<char>>()
        .chunks(40)
        .map(|line| line.iter().collect::<String>())
        .collect::<Vec<String>>()
        .join("\n  ");

    assert!(Brand::new(Some("Acme"), Some(&wrapped), None).is_ok());
}

/// The path is a cache key: the same bytes are the same address, and different
/// bytes are a different one, so a year-long cache never hands somebody the
/// last customer's logo.
#[test]
fn the_logo_path_follows_the_bytes() {
    let first = logo(PNG).logo.unwrap().path;
    let again = logo(PNG).logo.unwrap().path;
    let other = logo(&[PNG, &[0x42]].concat()).logo.unwrap().path;

    assert_eq!(first, again);
    assert_ne!(first, other);
    assert!(first.starts_with(LOGO_PREFIX));
}

/// Neither the name nor the picture belongs in a log line.
#[test]
fn the_debug_rendering_carries_no_bytes() {
    let rendered = format!("{:?}", logo(PNG));

    assert!(rendered.contains("Acme"), "the shape is what a log line is for");
    assert!(
        rendered.contains("/brand/logo-"),
        "a log line cannot say which logo is loaded: {rendered}"
    );
    assert!(
        rendered.len() < 200,
        "the picture itself reached a log line: {} bytes",
        rendered.len()
    );
}

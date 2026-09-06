//! The three moments of a sign-in that the browser actually sees: being sent
//! to the provider, coming back from it, and pressing Sign out.
//!
//! The shape is the one that keeps the room out of the browser's hands. A
//! person opens `/r/standup` with no session; this remembers *`/r/standup`* in
//! a map keyed by a random `state`, and sends them away. What comes back
//! carries the `state` and nothing else about where they were going -- not a
//! `next=` parameter, not a cookie holding a path -- so there is nothing for
//! anybody to edit into a redirect somewhere else, and the person lands on the
//! clean `/r/standup` they asked for with no trace of the sign-in in the
//! address bar. The callback is a `302` rather than a page that redirects
//! itself, so it leaves no entry in the back button either.
//!
//! ## What a callback has to prove
//!
//! Three things, and each of them is a separate way of getting this wrong:
//!
//!   * that this process started this sign-in -- the `state` names a login in
//!     a map only this process writes to, and it is spent on the way past, so a
//!     replayed callback finds nothing
//!   * that it came back to the browser it left from -- the login cookie holds
//!     a secret whose fingerprint was stored with the login. Without this
//!     somebody can send you a working callback URL and quietly sign you into
//!     *their* account, and then read whatever you do next
//!   * that the token is about this sign-in -- the `nonce`, checked in
//!     [`crate::oidc::validate_id_token`]
//!
//! ## When it goes wrong the answer is a page, not a redirect
//!
//! A sign-in that failed cannot be answered by sending somebody back where they
//! were going, because where they were going is what sent them here: they would
//! bounce between this server and the provider until a browser stopped them.
//! So every refusal here ends at a page with a sentence on it and a link back
//! to the front page, and the person decides whether to try again.

use axum::extract::{Query, State};
use axum::http::header::{COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::oidc::{ProviderError, local_path, safe_return_url};
use crate::session::{
    Login, Session, fingerprint, login_cookie, login_cookie_name, random_token, same_secret,
    session_cookie, spent_login_cookie, spent_session_cookie,
};
use crate::state::{Front, Shared};

/// The redirect URI registered with the provider. It is a constant because the
/// provider has it written down: changing it here without changing it there
/// ends every sign-in with the provider refusing the request, and the two have
/// to be looked at together.
pub const CALLBACK_PATH: &str = "/auth/callback";

pub const LOGOUT_PATH: &str = "/auth/logout";

const NO_STORE: &str = "no-store";

/// The provider's own name for "the person said no". It is not an error to
/// report as one: somebody pressed Cancel.
const ACCESS_DENIED: &str = "access_denied";

/// Whoever this request is from, if they are signed in.
///
/// A browser may hold more than one cookie of the same name -- one set for the
/// host and one for a parent domain, on a host that once had a different
/// configuration -- and a browser sends them all, in an order nothing
/// guarantees. Taking the first would let a stale one shadow the real one, so
/// every candidate is tried and the first that names a live session wins. The
/// `__Host-` prefix makes this nearly impossible; "nearly" is not a reason to
/// read only one of them.
pub fn signed_in(front: &Front, headers: &HeaderMap) -> Option<Session> {
    cookies(headers, crate::session::SESSION_COOKIE)
        .find_map(|cookie| front.sessions.lookup(cookie))
}

/// Sends a browser to the provider, remembering where it was going.
///
/// `uri` is the request this server is answering, so the path in it is a path
/// on this server; [`local_path`] is still asked, because `//evil.example` is
/// a path a request can carry and a `Location` built from it leaves this
/// origin.
pub async fn start_sign_in(front: &Front, uri: &Uri) -> Response {
    let Some(provider) = front.provider.as_ref() else {
        return page(
            StatusCode::NOT_FOUND,
            "Not found",
            "This server does not sign anybody in.",
        );
    };

    let return_to = uri
        .path_and_query()
        .map(ToString::to_string)
        .and_then(|path| local_path(&path))
        .unwrap_or_else(|| "/".to_owned());

    let secret = random_token();
    let login = Login::new(
        return_to,
        random_token(),
        random_token(),
        fingerprint(&secret),
    );
    let state = front.sessions.start_login(login.clone());

    match provider.authorize(&state, &login).await {
        Ok(url) => with_cookie(found(&url), &login_cookie(&state, &secret)),
        Err(error) => {
            tracing::error!(%error, "nobody can be sent to sign in");

            unreachable_page()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Where the provider sends the browser back to.
pub async fn callback(
    State(front): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(state) = present(query.state.as_deref()) else {
        tracing::warn!("a callback arrived with no state, so it belongs to no sign-in here");

        return refused_page();
    };

    // The login is spent whatever happens next. A callback that got this far
    // has used up its `state`, and leaving it in the map would leave a code
    // redeemable a second time by whoever else has the URL.
    let Some(login) = front.sessions.finish_login(state) else {
        tracing::warn!("a callback arrived with a state this process never issued or has spent");

        return refused_page();
    };

    let name = login_cookie_name(state);
    let proved =
        cookies(&headers, &name).any(|secret| same_secret(&fingerprint(secret), &login.browser));

    let answer = if !proved {
        tracing::warn!(
            "a callback arrived in a browser that cannot show the secret this sign-in started \
             with, so it is somebody else's sign-in being finished here"
        );

        refused_page()
    } else {
        complete(&front, &login, &query).await
    };

    with_cookie(answer, &spent_login_cookie(state))
}

async fn complete(front: &Front, login: &Login, query: &CallbackQuery) -> Response {
    if let Some(error) = present(query.error.as_deref()) {
        // Logged as the provider's own code and nothing else. An
        // `error_description` is a string from another system and this one has
        // no business rendering it.
        tracing::info!(%error, "the provider did not sign this person in");

        return match error {
            ACCESS_DENIED => page(
                StatusCode::FORBIDDEN,
                "Not signed in",
                "The sign-in was cancelled, so nobody was signed in here.",
            ),
            _ => page(
                StatusCode::FORBIDDEN,
                "Not signed in",
                "The identity provider would not sign you in. Try again, and if it keeps \
                 happening whoever runs the provider has the reason.",
            ),
        };
    }

    let (Some(provider), Some(code)) = (front.provider.as_ref(), present(query.code.as_deref()))
    else {
        tracing::warn!("a callback arrived with neither a code nor an error");

        return refused_page();
    };

    let identity = match provider.complete(code, login).await {
        Ok(identity) => identity,
        Err(error) => {
            let unreachable = matches!(error, ProviderError::Unreachable);
            tracing::warn!(%error, "this sign-in could not be finished");

            return if unreachable {
                unreachable_page()
            } else {
                page(
                    StatusCode::FORBIDDEN,
                    "Not signed in",
                    "This sign-in did not check out, so nobody was signed in. Go back to the \
                     front page and try again.",
                )
            };
        }
    };

    tracing::info!(
        subject = %identity.subject,
        roles = identity.roles.len(),
        "signed somebody in"
    );

    let cookie = front.sessions.open(identity);

    with_cookie(
        found(&login.return_to),
        &session_cookie(&cookie, front.sessions.ttl().as_secs()),
    )
}

#[derive(Debug, Deserialize)]
pub struct LogoutQuery {
    #[serde(rename = "returnUrl")]
    return_url: Option<String>,
}

/// Signs somebody out here, and then at the provider.
///
/// Both halves matter. Forgetting the session here and stopping there leaves
/// the provider's own session standing, so the next press of Join signs the
/// same person back in without a prompt and the button looks broken.
///
/// The `post_logout_redirect_uri` handed to the provider is always the
/// configured one, because that is the value registered with it and a provider
/// refuses any other. A `?returnUrl=` is therefore for the local half: it is
/// where somebody lands when the provider publishes no `end_session_endpoint`
/// or cannot be reached, and it is validated against this front end's own
/// origin first, because a redirect that forwards anywhere is a phishing link
/// wearing this host's name.
pub async fn logout(
    State(front): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<LogoutQuery>,
) -> Response {
    let Some(oidc) = front.config.oidc.as_ref() else {
        return page(
            StatusCode::NOT_FOUND,
            "Not found",
            "This server does not sign anybody in.",
        );
    };

    // Signing out is a navigation, not something another site gets to do to
    // you with an `<img>`. Browsers that send `Sec-Fetch-Site` say where the
    // request came from; browsers that do not are trusted, because refusing
    // them would break signing out rather than protect it.
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        && !matches!(site.trim(), "same-origin" | "none")
    {
        tracing::warn!(%site, "a sign-out was asked for by another site");

        return page(
            StatusCode::FORBIDDEN,
            "Not signed out",
            "Signing out has to be something you do here. Open this server's own page and press \
             Sign out there.",
        );
    }

    let here = safe_return_url(
        query.return_url.as_deref().unwrap_or_default(),
        &oidc.public_url,
    );
    let session = cookies(&headers, crate::session::SESSION_COOKIE)
        .find_map(|cookie| front.sessions.close(cookie));

    let target = match (session.as_ref(), front.provider.as_ref()) {
        (Some(session), Some(provider)) => provider
            .end_session(session.id_token(), &oidc.post_logout_uri())
            .await
            .unwrap_or(here),
        _ => here,
    };

    with_cookie(found(&target), &spent_session_cookie())
}

/// Every cookie of one name a request carries, in the order the browser sent
/// them.
pub fn cookies<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .filter(move |(key, _)| key.trim() == name)
        .filter_map(|(_, value)| present(Some(value)))
}

fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn found(url: &str) -> Response {
    match HeaderValue::from_str(url) {
        Ok(location) => (
            StatusCode::FOUND,
            [
                (LOCATION, location),
                (header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE)),
            ],
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "the redirect target is not something a header can carry");

            page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "This server could not work out where to send you next.",
            )
        }
    }
}

fn with_cookie(mut response: Response, cookie: &str) -> Response {
    match HeaderValue::from_str(cookie) {
        Ok(value) => {
            response.headers_mut().append(SET_COOKIE, value);
            response
        }
        Err(error) => {
            tracing::error!(%error, "a cookie is not something a header can carry");

            page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "This server could not set the cookie this sign-in needs.",
            )
        }
    }
}

fn refused_page() -> Response {
    page(
        StatusCode::BAD_REQUEST,
        "Not signed in",
        "This sign-in did not start here, or it has already finished. Go back to the front page \
         and try again.",
    )
}

fn unreachable_page() -> Response {
    page(
        StatusCode::SERVICE_UNAVAILABLE,
        "Nobody can sign in",
        "This server could not reach the identity provider, so nobody can be signed in right \
         now. Nothing is wrong with your account; try again in a moment.",
    )
}

/// A sentence and a way back, in the page's own stylesheet so it does not look
/// like a different server answered.
///
/// No inline style and no inline script: the content security policy this
/// binary sets allows neither, and an error page that trips it would be a blank
/// screen with a console message.
fn page(status: StatusCode, heading: &str, sentence: &str) -> Response {
    let body = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\" />\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
         <title>{heading} · mediacore</title>\n\
         <link rel=\"stylesheet\" href=\"/app.css\" />\n</head>\n<body>\n\
         <main class=\"page\">\n<section class=\"lobby\">\n<div class=\"card\">\n\
         <header class=\"brand\"><span class=\"brand-dot\"></span>\
         <span class=\"brand-name\">mediacore</span></header>\n\
         <h1>{heading}</h1>\n<p class=\"lede\">{sentence}</p>\n\
         <p class=\"status\"><a href=\"/\">Back to the front page</a></p>\n\
         </div>\n</section>\n</main>\n</body>\n</html>\n",
        heading = escaped(heading),
        sentence = escaped(sentence),
    );

    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE)),
        ],
        body,
    )
        .into_response()
}

/// Everything on these pages is a constant in this file, so nothing here is
/// escaping anybody's input. It escapes anyway, because the day one of these
/// sentences carries a room name or a provider's reason is the day the absence
/// of this function is a cross-site scripting hole.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn a_cookie_is_found_however_the_browser_packed_them() {
        let one = headers(&[("cookie", "a=1; __Host-mediacore_session=abc; b=2")]);
        assert_eq!(
            cookies(&one, "__Host-mediacore_session").collect::<Vec<_>>(),
            vec!["abc"]
        );

        let split = headers(&[
            ("cookie", "a=1"),
            ("cookie", "__Host-mediacore_session=abc"),
        ]);
        assert_eq!(
            cookies(&split, "__Host-mediacore_session").collect::<Vec<_>>(),
            vec!["abc"],
            "a browser that sends one Cookie header per cookie was not read"
        );
    }

    /// Two cookies of one name is what a host that changed configuration
    /// leaves behind. Reading only the first would let the dead one shadow the
    /// live one, and the person would be signed out with a valid session in
    /// their browser.
    #[test]
    fn every_cookie_of_a_name_is_offered_and_not_just_the_first() {
        let both = headers(&[(
            "cookie",
            "__Host-mediacore_session=stale; __Host-mediacore_session=live",
        )]);

        assert_eq!(
            cookies(&both, "__Host-mediacore_session").collect::<Vec<_>>(),
            vec!["stale", "live"]
        );
    }

    #[test]
    fn a_cookie_that_is_not_there_is_not_an_empty_one() {
        let none = headers(&[("cookie", "__Host-mediacore_session=; a=1")]);
        assert_eq!(
            cookies(&none, "__Host-mediacore_session").count(),
            0,
            "an empty cookie was offered as a session id"
        );
        assert_eq!(
            cookies(&HeaderMap::new(), "__Host-mediacore_session").count(),
            0
        );
    }

    #[test]
    fn a_page_this_server_writes_cannot_carry_markup_into_itself() {
        assert_eq!(
            escaped(r#"<script>alert("x")</script>"#),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"
        );
        assert_eq!(escaped("a & b"), "a &amp; b");
        assert_eq!(escaped("Роман"), "Роман");
    }

    #[test]
    fn the_registered_paths_are_the_ones_this_answers_at() {
        assert_eq!(CALLBACK_PATH, "/auth/callback");
        assert_eq!(LOGOUT_PATH, "/auth/logout");
    }
}

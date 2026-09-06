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
//!     a map only this process writes to
//!   * that it came back to the browser it left from -- the login cookie holds
//!     a secret whose fingerprint was stored with the login. Without this
//!     somebody can send you a working callback URL and quietly sign you into
//!     *their* account, and then read whatever you do next
//!   * that the token is about this sign-in -- the `nonce`, checked in
//!     [`crate::oidc::validate_id_token`]
//!
//! In that order, and the order is load bearing. The login is spent *after* the
//! browser has been proved, not on the way past: a callback that cannot show
//! the cookie has redeemed no code, so there is nothing to spend, and spending
//! anyway let anybody who had merely read a `state` -- from an address bar, from
//! the provider's access log -- end that sign-in by fetching one URL. Once the
//! cookie checks out the login is gone whatever happens next, which is the whole
//! of the protection against a code being redeemed twice.
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
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::log::quoted;
use crate::oidc::{ProviderError, local_path, safe_return_url};
use crate::session::{
    LOGIN_COOKIE, Login, NotFinished, Session, fingerprint, login_cookie, random_token,
    same_secret, session_cookie, spent_login_cookie, spent_session_cookie,
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

/// Whether this request is somebody arriving, rather than something another
/// page made this browser fetch.
///
/// A sign-in in flight costs this process an entry in a bounded map and costs
/// the browser a cookie, and only a person opening the page will ever finish
/// one. Everything else that reaches `/` or `/r/<room>` -- an `<img>` on
/// somebody else's page, a prefetch, a crawler that speaks `Sec-Fetch-*`, a
/// widget fetching the origin -- would start a sign-in nobody is at the other
/// end of, and doing that thousands of times is how the map was filled and the
/// cookie jar with it.
///
/// `Sec-Fetch-Dest: document` is most of the test: a top-level navigation, which
/// is what an address bar, a link and a redirect back from the provider all
/// produce. A browser that sends no such header is trusted, the same way
/// [`logout`] trusts one that sends no `Sec-Fetch-Site` -- refusing them would
/// break signing in for old browsers rather than protect anything, and a
/// scripted flood can set any header it likes anyway. What this closes is the
/// shape that needs no script at all: `<img src="https://host/r/a">` on a page
/// somebody else controls.
///
/// The method is the other half, and it is the half that catches what sends no
/// `Sec-Fetch-*` at all. Axum answers `HEAD` from the same handler as `GET`, and
/// a `HEAD` is never somebody about to type a password: it is a monitor, a link
/// unfurler or a crawler, every one of which would otherwise start a sign-in
/// nobody will ever come back to finish.
fn somebody_arriving(method: &Method, headers: &HeaderMap) -> bool {
    if method != Method::GET {
        return false;
    }

    let header = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };

    if let Some(destination) = header("sec-fetch-dest")
        && destination != "document"
    {
        return false;
    }

    // A page the browser fetched because it *might* be wanted. Chrome sends
    // `Sec-Purpose`, older Chrome `Purpose`, Firefox `X-moz`; none of them is a
    // person who is about to type a password.
    ![header("sec-purpose"), header("purpose"), header("x-moz")]
        .into_iter()
        .flatten()
        .any(|purpose| purpose.contains("prefetch") || purpose.contains("prerender"))
}

/// Sends a browser to the provider, remembering where it was going.
///
/// `uri` is the request this server is answering, so the path in it is a path
/// on this server; [`local_path`] is still asked, because `//evil.example` is
/// a path a request can carry and a `Location` built from it leaves this
/// origin.
///
/// The order of the last two steps is deliberate: the redirect is built first
/// and the login is remembered only once there is somewhere to send the person.
/// A provider having a bad ten minutes would otherwise fill the map with
/// sign-ins that never left this process.
pub async fn start_sign_in(
    front: &Front,
    method: &Method,
    headers: &HeaderMap,
    uri: &Uri,
) -> Response {
    let Some(provider) = front.provider.as_ref() else {
        return page(
            StatusCode::NOT_FOUND,
            "Not found",
            "This server does not sign anybody in.",
        );
    };

    if !somebody_arriving(method, headers) {
        return page(
            StatusCode::UNAUTHORIZED,
            "Not signed in",
            "This page signs you in when you open it yourself. Nothing was started for whatever \
             asked for it here.",
        );
    }

    let return_to = uri
        .path_and_query()
        .map(ToString::to_string)
        .and_then(|path| local_path(&path))
        .unwrap_or_else(|| "/".to_owned());

    // The secret this browser already holds, if it has one. Reusing it is what
    // keeps a browser to a single login cookie however many times it loads the
    // page, and it leaves any sign-in already in flight in this browser able to
    // finish -- the fingerprint stored with that login is of this same secret.
    let secret = cookies(headers, LOGIN_COOKIE)
        .next()
        .map_or_else(random_token, str::to_owned);

    let login = Login::new(
        return_to,
        random_token(),
        random_token(),
        fingerprint(&secret),
    );

    // 256 random bits, minted here rather than by the store because the
    // redirect is built before anything is remembered. It comes back in the
    // callback's query string, so it has to be unguessable and safe in a URL.
    let state = random_token();
    let url = match provider.authorize(&state, &login).await {
        Ok(url) => url,
        Err(error) => {
            tracing::error!(error = %quoted(&error), "nobody can be sent to sign in");

            return unreachable_page();
        }
    };

    if !front.sessions.start_login(&state, login) {
        return busy_page();
    }

    with_cookie(found(&url), &login_cookie(&secret))
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

    // The browser is checked and the login is spent in that order, under one
    // lock. A callback that cannot show the cookie has redeemed no code, so
    // there is nothing to spend -- and spending anyway is a way for anybody who
    // learned the `state` to end somebody else's sign-in without holding a
    // thing. Once the check passes the login is gone whatever happens next,
    // which is what stops a code being redeemed twice.
    let login = match front.sessions.finish_login(state, |login| {
        cookies(&headers, LOGIN_COOKIE)
            .any(|secret| same_secret(&fingerprint(secret), &login.browser))
    }) {
        Ok(login) => login,
        Err(NotFinished::Unknown) => {
            tracing::warn!(
                "a callback arrived with a state this process never issued or has spent"
            );

            return refused_page();
        }
        Err(NotFinished::WrongBrowser) => {
            tracing::warn!(
                "a callback arrived in a browser that cannot show the secret this sign-in started \
                 with, so it is somebody else's sign-in being finished here"
            );

            // No `Set-Cookie` on the way out either. This refusal is answered
            // to whoever asked, and whoever asked may be a browser that was
            // made to fetch the callback by a page somewhere else -- clearing
            // the login cookie there would take away the proof its own sign-in
            // is about to need.
            return refused_page();
        }
    };

    let answer = complete(&front, &login, &query).await;

    // The cookie is one per browser, so it is spent only when nothing else in
    // this browser is still relying on it: a second tab may be at the provider
    // with a login of its own whose proof is this same secret.
    match front.sessions.another_login_in(&login.browser) {
        true => answer,
        false => with_cookie(answer, &spent_login_cookie()),
    }
}

async fn complete(front: &Front, login: &Login, query: &CallbackQuery) -> Response {
    if let Some(error) = present(query.error.as_deref()) {
        // Logged as the provider's own code and nothing else. An
        // `error_description` is a string from another system and this one has
        // no business rendering it.
        //
        // Through `quoted` because this parameter is not the provider's: it
        // arrives in a query string anybody can write, and written straight
        // into the line a `%0a` in it wrote a *second* line that read as one of
        // this process's own. See `crate::log`.
        tracing::info!(error = %quoted(error), "the provider did not sign this person in");

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
            tracing::warn!(error = %quoted(&error), "this sign-in could not be finished");

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
        tracing::warn!(site = %quoted(site), "a sign-out was asked for by another site");

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

/// What somebody arriving during a flood is told.
///
/// It is deliberately the answer *this* person gets rather than one somebody
/// else gets: the alternative was making room by dropping a sign-in already in
/// flight, which reads as a working page for the newcomer and a refused
/// callback for whoever was halfway through typing a password. A person who
/// tries again in a minute gets in; a person thrown out mid-sign-in has no idea
/// what happened.
fn busy_page() -> Response {
    page(
        StatusCode::SERVICE_UNAVAILABLE,
        "Nobody can sign in",
        "This server is already holding as many sign-ins as it will hold at once, so this one \
         could not be started. Try again in a minute.",
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

//! Carrying a signalling connection to the node that holds the room.
//!
//! A client reaches whatever the load balancer picked, and the room it named is
//! held by one node in particular. Nothing in a LiveKit client can be told to
//! go somewhere else -- the protocol's `alternative_url` is decoded and thrown
//! away by every SDK in this tree -- so the node that was reached carries the
//! connection rather than redirecting it.
//!
//! **Only signalling comes this way.** The join response the client receives is
//! the owner's, so the ICE candidates in it are the owner's addresses and media
//! goes straight there, never through here. What this relays is the websocket:
//! a few kilobytes a minute, against a media path that is not touched at all.
//!
//! **One hop, never two.** The inner connection is marked, and a node that
//! receives a marked connection serves it or refuses it but never passes it on
//! again. Without that, a stale view on two nodes is an endless ring.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

/// The query parameter that says a connection has already been carried once.
///
/// A parameter and not a header, because it has to survive the same path the
/// token does and because a node reading it is reading the same query string it
/// already parses.
pub const RELAYED: &str = "mediacore_relayed";

/// How long the owner has to accept the inner connection.
///
/// Short: this runs while somebody is waiting to join, and a node that is not
/// answering its own cluster address in two seconds is a node whose rooms are
/// about to be somebody else's problem anyway.
const DIAL_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether this connection has already been carried.
pub fn already_relayed(query: &str) -> bool {
    form_pairs(query).any(|(key, value)| key == RELAYED && value == "1")
}

/// The address of the inner connection: the owner's, carrying the same query
/// this node was given plus the mark.
pub fn inner_url(signal_url: &str, query: &str) -> String {
    let base = signal_url.trim_end_matches('/');
    let separator = match query.is_empty() {
        true => "",
        false => "&",
    };

    format!("{base}/rtc?{query}{separator}{RELAYED}=1")
}

/// Pumps one websocket into another until either end goes.
///
/// Frames are passed through as they are. Nothing here reads a signal message:
/// this node is not a participant in the conversation and has no business
/// having an opinion about it, and a relay that decoded the protocol would be a
/// second place to keep it up to date.
pub async fn carry(client: WebSocket, url: String, token: String) -> Result<(), String> {
    // The pass goes on the inner connection as a header regardless of how the
    // client sent it. A query string would work too, but only for the clients
    // that use one: the newer server SDKs send `Authorization` and nothing in
    // the query, so a relay that only copied the query would carry them to a
    // node that answers 401 -- which is exactly what it did before this line.
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|error| format!("the address of the node holding this room is not one: {error}"))?;

    let bearer = format!("Bearer {token}")
        .parse()
        .map_err(|_| "the pass on this connection cannot go in a header".to_owned())?;
    request.headers_mut().insert(AUTHORIZATION, bearer);

    let dial = tokio_tungstenite::connect_async(request);

    let (owner, _) = tokio::time::timeout(DIAL_TIMEOUT, dial)
        .await
        .map_err(|_| format!("the node holding this room did not answer in time: {url}"))?
        .map_err(|error| format!("the node holding this room refused the connection: {error}"))?;

    let (mut to_owner, mut from_owner) = owner.split();
    let (mut to_client, mut from_client) = client.split();

    // Both directions run until one of them ends. Whichever goes first takes
    // the other with it: half a relayed connection is worse than none, because
    // the client would sit waiting on a socket nobody is answering.
    let upstream = async {
        while let Some(message) = from_client.next().await {
            let Ok(message) = message else { break };

            let frame = match message {
                Message::Binary(data) => Frame::Binary(data),
                Message::Text(text) => Frame::Text(text.as_str().into()),
                Message::Ping(data) => Frame::Ping(data),
                Message::Pong(data) => Frame::Pong(data),
                Message::Close(_) => break,
            };

            if to_owner.send(frame).await.is_err() {
                break;
            }
        }

        let _ = to_owner.close().await;
    };

    let downstream = async {
        while let Some(frame) = from_owner.next().await {
            let Ok(frame) = frame else { break };

            let message = match frame {
                Frame::Binary(data) => Message::Binary(data),
                Frame::Text(text) => Message::Text(text.as_str().into()),
                Frame::Ping(data) => Message::Ping(data),
                Frame::Pong(data) => Message::Pong(data),
                Frame::Close(_) => break,
                Frame::Frame(_) => continue,
            };

            if to_client.send(message).await.is_err() {
                break;
            }
        }

        let _ = to_client.close().await;
    };

    tokio::select! {
        _ = upstream => {}
        _ = downstream => {}
    }

    Ok(())
}

/// `key=value` pairs out of a query string, percent-decoding nothing.
///
/// Enough for the one flag read here, whose value is `1`. The token is read by
/// the code that already parses this query properly; this does not touch it.
fn form_pairs(query: &str) -> impl Iterator<Item = (&str, &str)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use mediacore_proto::livekit::{self, signal_response};
use mediacore_room::{Joined, TrackKind};
use mediacore_transport::str0m::format::{Codec, PayloadParams};
use mediacore_transport::str0m::media::{Direction, MediaKind, Mid, Pt};
use mediacore_transport::{MediaSpec, Negotiation, PeerEvent, PeerEvents, PeerHandle, SendStream};
use tokio::task::JoinHandle;

use crate::forward::{SinkConfig, TrackEffects, TrackSink, forwarder_config, stream_start};
use crate::peer::{bundle_mid, candidate_from_init, candidate_init};
use crate::simulcast::{apply_quality_update, preference_of};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Negotiating,
    Bound,
    Waiting,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Live,
    Idle,
}

struct Subscription {
    publisher_sid: String,
    mid: Mid,
    kind: TrackKind,
    liveness: Liveness,
    paused: bool,
    sink: Arc<TrackSink>,
}

struct Pending {
    track_sid: String,
    publisher_sid: String,
    mid: Mid,
    kind: TrackKind,
    mime_type: String,
    id: u32,
    since: Instant,
}

pub struct SubscriberSession {
    handle: Option<PeerHandle>,
    events: Option<PeerEvents>,
    task: Option<JoinHandle<()>>,
    queued_candidates: Vec<String>,
    subscriptions: HashMap<String, Subscription>,
    settings: HashMap<String, livekit::UpdateTrackSettings>,
    tracks_by_mid: HashMap<Mid, String>,
    pending: Option<Pending>,
    auto_subscribe: bool,
    auto_subscribed: HashSet<String>,
    candidates_sent: bool,
    offers: u32,
    failed: bool,
}

impl SubscriberSession {
    pub fn new(auto_subscribe: bool) -> Self {
        Self {
            handle: None,
            events: None,
            task: None,
            queued_candidates: Vec::new(),
            subscriptions: HashMap::new(),
            settings: HashMap::new(),
            tracks_by_mid: HashMap::new(),
            pending: None,
            auto_subscribe,
            auto_subscribed: HashSet::new(),
            candidates_sent: false,
            offers: 0,
            failed: false,
        }
    }

    pub async fn next_event(&mut self) -> Option<PeerEvent> {
        match self.events.as_mut() {
            Some(events) => events.recv().await,
            None => std::future::pending().await,
        }
    }

    pub fn on_event(&mut self, state: &AppState, joined: &Joined, event: PeerEvent) -> bool {
        match event {
            PeerEvent::Connected => {
                tracing::info!(
                    identity = %joined.participant.identity,
                    "subscriber transport connected"
                );
            }
            PeerEvent::IceConnectionState(ice) => {
                tracing::debug!(
                    identity = %joined.participant.identity,
                    ?ice,
                    "subscriber ice state"
                );
            }
            PeerEvent::KeyframeRequested(request) => {
                let Some(track_sid) = self.tracks_by_mid.get(&request.mid) else {
                    return true;
                };

                tracing::debug!(
                    identity = %joined.participant.identity,
                    track = %track_sid,
                    kind = ?request.kind,
                    "subscriber asked for a keyframe"
                );
                state
                    .router
                    .request_keyframe_for(track_sid, &joined.participant.sid, false);
            }
            PeerEvent::EgressReport(report) => {
                if let Some(track_sid) = self.tracks_by_mid.get(&report.mid) {
                    tracing::trace!(
                        identity = %joined.participant.identity,
                        track = %track_sid,
                        rtt = ?report.rtt,
                        loss = ?report.loss,
                        nacks = report.nacks,
                        plis = report.plis,
                        "subscriber egress report"
                    );
                }
            }
            PeerEvent::NegotiationFailed => {
                if self.pending.is_some() {
                    self.fail_negotiation(joined, "the transport gave up on an unanswered offer");
                } else {
                    tracing::debug!(
                        identity = %joined.participant.identity,
                        "a negotiation failure arrived after the answer landed, ignoring it"
                    );
                }
            }
            PeerEvent::MediaAdded(_)
            | PeerEvent::MediaChanged(_)
            | PeerEvent::Rtp(_)
            | PeerEvent::Media(_)
            | PeerEvent::IngressReport(_)
            | PeerEvent::LayerActivity(_) => {}
            PeerEvent::Disconnected => {
                tracing::info!(
                    identity = %joined.participant.identity,
                    "subscriber transport disconnected"
                );
                return false;
            }
        }

        true
    }

    pub async fn on_room_changed(&mut self, state: &AppState, joined: &Joined) {
        self.refresh_auto_subscriptions(joined);
        self.follow_publisher_mutes(joined);
        self.reconcile(state, joined).await;
    }

    pub async fn on_answer(
        &mut self,
        state: &AppState,
        joined: &Joined,
        answer: livekit::SessionDescription,
    ) {
        let Some(pending) = self.pending.take() else {
            tracing::debug!(
                identity = %joined.participant.identity,
                "an answer arrived while no subscriber offer was outstanding"
            );
            return;
        };

        if answer.id != 0 && answer.id != pending.id {
            tracing::debug!(
                identity = %joined.participant.identity,
                answered = answer.id,
                offered = pending.id,
                "the answer names another offer, applying it to the outstanding one"
            );
        }

        let Some(handle) = self.handle.clone() else {
            return;
        };

        if let Err(error) = handle.accept_answer(answer.sdp).await {
            tracing::warn!(
                identity = %joined.participant.identity,
                track = %pending.track_sid,
                %error,
                "the subscriber answer was refused"
            );
            self.tracks_by_mid.remove(&pending.mid);
            self.fail_negotiation(
                joined,
                "the subscriber answered with an sdp we cannot apply",
            );
            return;
        }

        self.bind(state, joined, pending).await;
        self.reconcile(state, joined).await;
    }

    pub async fn on_trickle(&mut self, joined: &Joined, request: livekit::TrickleRequest) {
        let Some(candidate) = candidate_from_init(&request.candidate_init) else {
            return;
        };

        let Some(handle) = self.handle.clone() else {
            tracing::debug!(
                identity = %joined.participant.identity,
                "holding a candidate until the subscriber transport exists"
            );
            self.queued_candidates.push(candidate);
            return;
        };

        if let Err(error) = handle.add_remote_candidate(candidate).await {
            tracing::debug!(
                identity = %joined.participant.identity,
                %error,
                "rejected a remote subscriber candidate"
            );
        }
    }

    pub async fn on_track_settings(
        &mut self,
        state: &AppState,
        joined: &Joined,
        settings: &livekit::UpdateTrackSettings,
    ) {
        for track_sid in &settings.track_sids {
            if joined.room.track(track_sid).is_none() {
                tracing::debug!(
                    identity = %joined.participant.identity,
                    track = %track_sid,
                    "settings for a track nobody publishes"
                );
                continue;
            }

            self.settings.insert(track_sid.clone(), settings.clone());
            self.apply_settings(state, joined, track_sid).await;
        }
    }

    async fn apply_settings(&mut self, state: &AppState, joined: &Joined, track_sid: &str) {
        let Some(settings) = self.settings.get(track_sid).cloned() else {
            return;
        };
        let subscriber_sid = &joined.participant.sid;

        let Some(subscription) = self.subscriptions.get_mut(track_sid) else {
            tracing::debug!(
                identity = %joined.participant.identity,
                track = %track_sid,
                "holding settings for a track this participant is not subscribed to yet"
            );
            return;
        };

        let publisher_sid = subscription.publisher_sid.clone();
        let kind = subscription.kind;
        let toggled = subscription.paused != settings.disabled;
        subscription.paused = settings.disabled;

        let mut effects = state
            .router
            .set_paused(track_sid, subscriber_sid, settings.disabled);

        if kind == TrackKind::Video && !settings.disabled {
            let preference = preference_of(&settings);

            tracing::debug!(
                identity = %joined.participant.identity,
                track = %track_sid,
                %preference,
                "subscriber asked for another layer"
            );
            effects.merge(
                state
                    .router
                    .set_preference(track_sid, subscriber_sid, preference),
            );
        }

        if toggled {
            stream_state(joined, &publisher_sid, track_sid, settings.disabled);
        }

        self.flush(state, joined, track_sid, effects).await;
    }

    async fn flush(
        &self,
        state: &AppState,
        joined: &Joined,
        track_sid: &str,
        effects: TrackEffects,
    ) {
        if effects.is_empty() {
            return;
        }

        if let Some(update) = state
            .router
            .pending_quality_update(track_sid, Instant::now())
        {
            apply_quality_update(state, &joined.room, &update).await;
        }
    }

    pub async fn reconcile(&mut self, state: &AppState, joined: &Joined) {
        self.expire_pending(state, joined);

        if self.failed {
            return;
        }

        self.forget_departed_tracks(state, joined).await;

        let diff = joined.participant.subscription_diff();

        for track_sid in diff.to_unsubscribe {
            self.teardown(state, joined, &track_sid).await;
        }

        if self.pending.is_some() {
            return;
        }

        for track_sid in diff.to_subscribe {
            if self.subscribe(state, joined, &track_sid).await == Step::Negotiating {
                return;
            }
        }
    }

    pub fn close(&mut self, state: &AppState, joined: &Joined) {
        state.router.remove_subscriber(&joined.participant.sid);

        if let Some(handle) = self.handle.take() {
            handle.close();
        }

        self.events = None;
        self.task = None;
        self.subscriptions.clear();
        self.settings.clear();
        self.tracks_by_mid.clear();
        self.auto_subscribed.clear();
        self.pending = None;
    }

    fn refresh_auto_subscriptions(&mut self, joined: &Joined) {
        if !self.auto_subscribe {
            return;
        }

        let fresh: Vec<String> = joined
            .room
            .subscribable_track_sids(&joined.participant.sid)
            .into_iter()
            .filter(|track_sid| self.auto_subscribed.insert(track_sid.clone()))
            .collect();

        if fresh.is_empty() {
            return;
        }

        tracing::debug!(
            identity = %joined.participant.identity,
            tracks = ?fresh,
            "auto-subscribing to newly visible tracks"
        );
        joined.participant.add_desired_subscriptions(fresh);
    }

    fn follow_publisher_mutes(&self, joined: &Joined) {
        for (track_sid, subscription) in &self.subscriptions {
            let Some((_, track)) = joined.room.track(track_sid) else {
                continue;
            };

            subscription.sink.set_publisher_muted(track.muted);
        }
    }

    fn expire_pending(&mut self, state: &AppState, joined: &Joined) {
        let timeout = state.media.negotiation_timeout();
        let expired = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.since.elapsed() >= timeout);

        if !expired {
            return;
        }

        let Some(pending) = self.pending.take() else {
            return;
        };

        tracing::warn!(
            identity = %joined.participant.identity,
            track = %pending.track_sid,
            mid = %pending.mid,
            ?timeout,
            "the subscriber never answered our offer"
        );

        self.fail_negotiation(joined, "the subscriber never answered our offer");
    }

    fn fail_negotiation(&mut self, joined: &Joined, reason: &'static str) {
        if self.failed {
            return;
        }

        self.failed = true;
        self.pending = None;

        tracing::warn!(
            identity = %joined.participant.identity,
            reason,
            "the subscriber media sections can no longer be trusted, asking for a full reconnect"
        );

        joined.participant.send(mediacore_room::reconnect(
            livekit::DisconnectReason::StateMismatch,
        ));
    }

    async fn forget_departed_tracks(&mut self, state: &AppState, joined: &Joined) {
        let departed: Vec<String> = self
            .subscriptions
            .keys()
            .filter(|track_sid| joined.room.track(track_sid).is_none())
            .cloned()
            .collect();

        for track_sid in departed {
            self.drop_subscription(state, joined, &track_sid).await;
        }
    }

    async fn drop_subscription(&mut self, state: &AppState, joined: &Joined, track_sid: &str) {
        joined.participant.mark_subscription_inactive(track_sid);
        joined
            .participant
            .remove_desired_subscriptions([track_sid.to_owned()]);
        self.auto_subscribed.remove(track_sid);
        self.settings.remove(track_sid);

        let Some(subscription) = self.subscriptions.remove(track_sid) else {
            return;
        };

        subscription.sink.set_paused(true);
        self.tracks_by_mid.remove(&subscription.mid);
        state.router.remove_sink(track_sid, &joined.participant.sid);

        if let Some(handle) = self.handle.clone()
            && let Err(error) = handle.remove_send_stream(subscription.mid, None).await
        {
            tracing::debug!(
                identity = %joined.participant.identity,
                track = %track_sid,
                mid = %subscription.mid,
                %error,
                "cannot free the outgoing stream of a dropped subscription"
            );
        }

        tracing::info!(
            identity = %joined.participant.identity,
            track = %track_sid,
            mid = %subscription.mid,
            "subscription dropped, the track it carried is gone"
        );
    }

    async fn subscribe(&mut self, state: &AppState, joined: &Joined, track_sid: &str) -> Step {
        if self.failed {
            return Step::Refused;
        }

        let Some((publisher, track)) = joined.room.track(track_sid) else {
            tracing::debug!(
                identity = %joined.participant.identity,
                track = %track_sid,
                "dropping a subscription to a track that is no longer published"
            );
            joined
                .participant
                .remove_desired_subscriptions([track_sid.to_owned()]);
            refuse(
                joined,
                track_sid,
                livekit::SubscriptionError::SeTrackNotfound,
            );
            return Step::Refused;
        };

        if !joined
            .room
            .can_subscribe(&joined.participant.sid, track_sid)
        {
            tracing::debug!(
                identity = %joined.participant.identity,
                track = %track_sid,
                "waiting for permission to subscribe"
            );
            return Step::Waiting;
        }

        if let Some(subscription) = self.subscriptions.get(track_sid) {
            if subscription.liveness == Liveness::Live {
                joined.participant.mark_subscription_active(track_sid);
                return Step::Bound;
            }

            return self.resume(state, joined, track_sid).await;
        }

        let Some(source) = state.router.source(track_sid) else {
            tracing::trace!(
                identity = %joined.participant.identity,
                track = %track_sid,
                "the publisher media is not live yet"
            );
            return Step::Waiting;
        };

        let Some(handle) = self.ensure_peer(state, joined).await else {
            return Step::Refused;
        };

        let kind = track.kind;
        let spec = MediaSpec {
            kind: media_kind(kind),
            direction: Direction::SendOnly,
            stream_id: Some(publisher.sid.clone()),
            track_id: Some(track.sid.clone()),
            simulcast: None,
        };

        let offer = match handle.negotiate(Negotiation::add(spec)).await {
            Ok(offer) => offer,
            Err(error) => {
                tracing::warn!(
                    identity = %joined.participant.identity,
                    track = %track_sid,
                    %error,
                    "cannot offer a subscription"
                );
                return Step::Waiting;
            }
        };

        let Some(mid) = offer.mids.first().copied() else {
            tracing::warn!(
                identity = %joined.participant.identity,
                track = %track_sid,
                "the subscriber offer carries no media section"
            );
            return Step::Waiting;
        };

        self.offers = self.offers.wrapping_add(1);
        self.tracks_by_mid.insert(mid, track_sid.to_owned());
        self.pending = Some(Pending {
            track_sid: track_sid.to_owned(),
            publisher_sid: publisher.sid.clone(),
            mid,
            kind,
            mime_type: source.mime_type().to_owned(),
            id: self.offers,
            since: Instant::now(),
        });

        tracing::info!(
            identity = %joined.participant.identity,
            track = %track_sid,
            publisher = %publisher.identity,
            %mid,
            "offering a subscription"
        );

        let bundle = bundle_mid(&offer.sdp);
        self.send_offer(joined, offer.sdp, self.offers);
        self.send_local_candidates(state, joined, &bundle);

        Step::Negotiating
    }

    async fn resume(&mut self, state: &AppState, joined: &Joined, track_sid: &str) -> Step {
        let Some(subscription) = self.subscriptions.get_mut(track_sid) else {
            return Step::Waiting;
        };

        subscription.liveness = Liveness::Live;
        subscription.paused = false;
        subscription.sink.set_paused(false);

        let sink = Arc::clone(&subscription.sink);
        let publisher_sid = subscription.publisher_sid.clone();

        state.router.add_sink(sink);
        joined.participant.mark_subscription_active(track_sid);
        state
            .router
            .request_keyframe_for(track_sid, &joined.participant.sid, true);

        tracing::info!(
            identity = %joined.participant.identity,
            track = %track_sid,
            "resuming a subscription on the media section it already owns"
        );

        self.announce(state, joined, track_sid, &publisher_sid);

        if self.settings.contains_key(track_sid) {
            self.apply_settings(state, joined, track_sid).await;
        }

        self.flush(state, joined, track_sid, TrackEffects::demand())
            .await;

        Step::Bound
    }

    async fn bind(&mut self, state: &AppState, joined: &Joined, pending: Pending) {
        let Some(handle) = self.handle.clone() else {
            return;
        };

        if let Err(error) = handle
            .declare_send_stream(SendStream::new(pending.mid))
            .await
        {
            tracing::warn!(
                identity = %joined.participant.identity,
                track = %pending.track_sid,
                %error,
                "cannot declare the outgoing stream for a subscription"
            );
            self.tracks_by_mid.remove(&pending.mid);
            return;
        }

        let media = match handle.media(pending.mid).await {
            Ok(media) => media,
            Err(error) => {
                tracing::warn!(
                    identity = %joined.participant.identity,
                    track = %pending.track_sid,
                    %error,
                    "the answered media section is unknown to the transport"
                );
                self.tracks_by_mid.remove(&pending.mid);
                return;
            }
        };

        let Some((pt, clock_rate)) = payload_type(&media.params, &pending.mime_type) else {
            tracing::warn!(
                identity = %joined.participant.identity,
                track = %pending.track_sid,
                mime_type = %pending.mime_type,
                "the subscriber did not accept the codec this track is published in"
            );
            joined
                .participant
                .remove_desired_subscriptions([pending.track_sid.clone()]);
            refuse(
                joined,
                &pending.track_sid,
                livekit::SubscriptionError::SeCodecUnsupported,
            );
            self.tracks_by_mid.remove(&pending.mid);
            return;
        };

        let start = stream_start(
            &joined.participant.sid,
            &pending.track_sid,
            u64::from(pending.id),
        );
        let sink = Arc::new(TrackSink::new(
            &pending.track_sid,
            &joined.participant.sid,
            handle,
            pending.mid,
            pt,
            SinkConfig {
                forwarder: forwarder_config(pending.kind, clock_rate),
                selector: state.selector_config(),
                start,
            },
        ));

        if let Some((_, track)) = joined.room.track(&pending.track_sid) {
            sink.set_publisher_muted(track.muted);
        }

        let subscribers = state.router.add_sink(Arc::clone(&sink));
        self.subscriptions.insert(
            pending.track_sid.clone(),
            Subscription {
                publisher_sid: pending.publisher_sid.clone(),
                mid: pending.mid,
                kind: pending.kind,
                liveness: Liveness::Live,
                paused: false,
                sink,
            },
        );

        joined
            .participant
            .mark_subscription_active(&pending.track_sid);
        state
            .router
            .request_keyframe_for(&pending.track_sid, &joined.participant.sid, true);

        tracing::info!(
            identity = %joined.participant.identity,
            track = %pending.track_sid,
            mid = %pending.mid,
            pt = *pt,
            "subscription is live"
        );

        self.announce(state, joined, &pending.track_sid, &pending.publisher_sid);

        if self.settings.contains_key(&pending.track_sid) {
            self.apply_settings(state, joined, &pending.track_sid).await;
        }

        self.flush(state, joined, &pending.track_sid, TrackEffects::demand())
            .await;

        tracing::debug!(
            identity = %joined.participant.identity,
            track = %pending.track_sid,
            subscribers,
            "the track has a new subscriber"
        );
    }

    async fn teardown(&mut self, state: &AppState, joined: &Joined, track_sid: &str) {
        joined.participant.mark_subscription_inactive(track_sid);

        let Some(subscription) = self.subscriptions.get_mut(track_sid) else {
            return;
        };

        if subscription.liveness == Liveness::Idle {
            return;
        }

        subscription.liveness = Liveness::Idle;
        subscription.sink.set_paused(true);

        let mid = subscription.mid;
        let remaining = state.router.remove_sink(track_sid, &joined.participant.sid);

        tracing::info!(
            identity = %joined.participant.identity,
            track = %track_sid,
            %mid,
            remaining,
            "subscription torn down"
        );

        self.flush(state, joined, track_sid, TrackEffects::demand())
            .await;
    }

    fn announce(&self, state: &AppState, joined: &Joined, track_sid: &str, publisher_sid: &str) {
        stream_state(joined, publisher_sid, track_sid, false);

        if !joined.participant.permission.hidden && state.router.mark_subscribed(track_sid) {
            let Some(publisher) = joined.room.participant_by_sid(publisher_sid) else {
                return;
            };

            publisher.send(signal_response::Message::TrackSubscribed(
                livekit::TrackSubscribed {
                    track_sid: track_sid.to_owned(),
                },
            ));
        }
    }

    async fn ensure_peer(&mut self, state: &AppState, joined: &Joined) -> Option<PeerHandle> {
        if let Some(handle) = self.handle.clone() {
            return Some(handle);
        }

        let peer = match state.media.spawn_subscriber() {
            Ok(peer) => peer,
            Err(error) => {
                tracing::error!(
                    identity = %joined.participant.identity,
                    %error,
                    "cannot start the subscriber transport"
                );
                return None;
            }
        };

        tracing::info!(
            identity = %joined.participant.identity,
            id = %peer.handle.id(),
            ufrag = %peer.handle.ice_creds().ufrag,
            "subscriber transport started"
        );

        self.handle = Some(peer.handle.clone());
        self.events = Some(peer.events);
        self.task = Some(peer.task);

        for candidate in std::mem::take(&mut self.queued_candidates) {
            if let Err(error) = peer.handle.add_remote_candidate(candidate).await {
                tracing::debug!(
                    identity = %joined.participant.identity,
                    %error,
                    "rejected a queued remote subscriber candidate"
                );
            }
        }

        Some(peer.handle)
    }

    fn send_offer(&self, joined: &Joined, sdp: String, id: u32) {
        let mid_to_track_id = self
            .tracks_by_mid
            .iter()
            .map(|(mid, track_sid)| (mid.to_string(), track_sid.clone()))
            .collect();

        joined.participant.send(signal_response::Message::Offer(
            livekit::SessionDescription {
                r#type: "offer".to_owned(),
                sdp,
                id,
                mid_to_track_id,
            },
        ));
    }

    fn send_local_candidates(&mut self, state: &AppState, joined: &Joined, bundle: &str) {
        if self.candidates_sent {
            return;
        }

        let Some((last, rest)) = state.media.candidates().split_last() else {
            tracing::warn!(
                identity = %joined.participant.identity,
                "no local ice candidate to trickle to the subscriber"
            );
            return;
        };

        for candidate in rest {
            joined
                .participant
                .send(trickle(candidate_init(candidate, bundle), false));
        }

        joined
            .participant
            .send(trickle(candidate_init(last, bundle), true));
        self.candidates_sent = true;
    }
}

fn trickle(candidate_init: String, final_candidate: bool) -> signal_response::Message {
    signal_response::Message::Trickle(livekit::TrickleRequest {
        candidate_init,
        target: livekit::SignalTarget::Subscriber as i32,
        r#final: final_candidate,
    })
}

fn refuse(joined: &Joined, track_sid: &str, error: livekit::SubscriptionError) {
    joined
        .participant
        .send(signal_response::Message::SubscriptionResponse(
            livekit::SubscriptionResponse {
                track_sid: track_sid.to_owned(),
                err: error as i32,
            },
        ));
}

fn stream_state(joined: &Joined, publisher_sid: &str, track_sid: &str, paused: bool) {
    let state = if paused {
        livekit::StreamState::Paused
    } else {
        livekit::StreamState::Active
    };

    joined
        .participant
        .send(signal_response::Message::StreamStateUpdate(
            livekit::StreamStateUpdate {
                stream_states: vec![livekit::StreamStateInfo {
                    participant_sid: publisher_sid.to_owned(),
                    track_sid: track_sid.to_owned(),
                    state: state as i32,
                }],
            },
        ));
}

fn media_kind(kind: TrackKind) -> MediaKind {
    match kind {
        TrackKind::Audio => MediaKind::Audio,
        TrackKind::Video => MediaKind::Video,
    }
}

fn payload_type(params: &[PayloadParams], mime_type: &str) -> Option<(Pt, u32)> {
    let name = mime_type.rsplit('/').next().unwrap_or(mime_type);

    params
        .iter()
        .find(|params| {
            let codec = params.spec().codec;
            !matches!(codec, Codec::Rtx | Codec::Null | Codec::Unknown)
                && codec.to_string().eq_ignore_ascii_case(name)
        })
        .map(|params| (params.pt(), params.spec().clock_rate.get()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use mediacore_config::Config;
    use mediacore_room::{ParticipantSpec, SignalRx};
    use mediacore_transport::str0m::format::{CodecSpec, FormatParams};
    use mediacore_transport::str0m::media::Frequency;

    use crate::forward::TrackSource;
    use crate::simulcast::LayerMap;

    const ROOM: &str = "demo";

    async fn app_state(negotiation_timeout: u32) -> AppState {
        let mut config =
            Config::parse("keys:\n  devkey: devsecretdevsecretdevsecretdevsecret\n").unwrap();
        config.port = 0;
        config.rtc.udp_port = 0;
        config.rtc.port_range_start = 0;
        config.rtc.port_range_end = 0;
        config.rtc.bind_address = "127.0.0.1".to_owned();
        config.rtc.negotiation_timeout = negotiation_timeout;

        AppState::start(config)
            .await
            .expect("the media engine starts")
    }

    fn spec(identity: &str) -> ParticipantSpec {
        ParticipantSpec {
            identity: identity.to_owned(),
            name: identity.to_owned(),
            permission: livekit::ParticipantPermission {
                can_subscribe: true,
                can_publish: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn add_track(cid: &str) -> livekit::AddTrackRequest {
        livekit::AddTrackRequest {
            cid: cid.to_owned(),
            name: cid.to_owned(),
            r#type: livekit::TrackType::Video as i32,
            source: livekit::TrackSource::Camera as i32,
            ..Default::default()
        }
    }

    fn drain(rx: &mut SignalRx) {
        while rx.try_recv().is_ok() {}
    }

    fn offers(rx: &mut SignalRx) -> Vec<livekit::SessionDescription> {
        let mut offers = Vec::new();

        while let Ok(response) = rx.try_recv() {
            if let Some(signal_response::Message::Offer(offer)) = response.message {
                offers.push(offer);
            }
        }

        offers
    }

    fn params(pt: u8, codec: Codec, clock_rate: Frequency) -> PayloadParams {
        PayloadParams::new(
            pt.into(),
            None,
            CodecSpec {
                codec,
                clock_rate,
                channels: None,
                format: FormatParams::default(),
            },
        )
    }

    #[test]
    fn the_payload_type_is_the_one_the_subscriber_negotiated_for_that_codec() {
        let offered = [
            params(111, Codec::Opus, Frequency::FORTY_EIGHT_KHZ),
            params(96, Codec::Vp8, Frequency::NINETY_KHZ),
            params(97, Codec::Rtx, Frequency::NINETY_KHZ),
        ];

        assert_eq!(
            payload_type(&offered, "video/VP8"),
            Some((96u8.into(), 90_000))
        );
        assert_eq!(
            payload_type(&offered, "audio/opus"),
            Some((111u8.into(), 48_000))
        );
        assert_eq!(payload_type(&offered, "video/H264"), None);
        assert_eq!(payload_type(&[], "audio/opus"), None);
    }

    #[tokio::test]
    async fn an_unanswered_subscriber_offer_never_moves_the_track_to_another_mid() {
        let state = app_state(0).await;
        let publisher = state
            .media
            .spawn_publisher()
            .expect("a publisher transport");

        let (alice_tx, _alice_rx) = mediacore_room::signal_channel();
        let (bob_tx, mut bob_rx) = mediacore_room::signal_channel();
        let alice = state
            .registry
            .join(ROOM, spec("alice"), alice_tx)
            .expect("alice joins");
        let bob = state
            .registry
            .join(ROOM, spec("bob"), bob_tx)
            .expect("bob joins");

        let track = alice
            .room
            .publish_track(&alice.participant.sid, &add_track("cid-camera"))
            .expect("alice publishes");
        state.router.register_source(
            &track.sid,
            TrackSource::new(
                publisher.handle.clone(),
                Mid::from("0"),
                TrackKind::Video,
                "video/VP8",
                &alice.participant.sid,
                LayerMap::build(&track.layers, &[]),
                state.source_config(track.source),
            ),
        );

        let mut session = SubscriberSession::new(false);
        bob.participant
            .add_desired_subscriptions([track.sid.clone()]);
        drain(&mut bob_rx);

        session.reconcile(&state, &bob).await;

        let offered = offers(&mut bob_rx);
        assert_eq!(offered.len(), 1, "the subscription was never offered");
        let first_mid = session
            .pending
            .as_ref()
            .expect("an offer is outstanding")
            .mid;

        session.reconcile(&state, &bob).await;

        assert!(
            offers(&mut bob_rx).is_empty(),
            "the track was re-offered on a media section the client never saw, \
             leaving mid {first_mid} orphaned in the client's peer connection"
        );
        assert!(session.pending.is_none());

        session.reconcile(&state, &bob).await;
        assert!(offers(&mut bob_rx).is_empty());

        publisher.handle.close();
    }

    #[tokio::test]
    async fn a_negotiation_failure_after_the_answer_landed_is_ignored() {
        let state = app_state(15).await;
        let (tx, mut rx) = mediacore_room::signal_channel();
        let joined = state
            .registry
            .join(ROOM, spec("bob"), tx)
            .expect("bob joins");

        let mut session = SubscriberSession::new(false);
        assert!(session.pending.is_none());
        drain(&mut rx);

        assert!(session.on_event(&state, &joined, PeerEvent::NegotiationFailed));

        assert!(
            !session.failed,
            "a stale negotiation failure must not condemn a healthy session"
        );
        assert!(
            rx.try_recv().is_err(),
            "the session was told to reconnect after its answer had already landed"
        );
    }

    #[tokio::test]
    async fn a_subscriber_that_never_answers_is_asked_to_reconnect() {
        let state = app_state(0).await;
        let (tx, mut rx) = mediacore_room::signal_channel();
        let joined = state
            .registry
            .join(ROOM, spec("bob"), tx)
            .expect("bob joins");

        let mut session = SubscriberSession::new(false);
        let mid = Mid::from("0");
        session.tracks_by_mid.insert(mid, "TR_camera".to_owned());
        session.pending = Some(Pending {
            track_sid: "TR_camera".to_owned(),
            publisher_sid: "PA_alice".to_owned(),
            mid,
            kind: TrackKind::Video,
            mime_type: "video/VP8".to_owned(),
            id: 1,
            since: Instant::now(),
        });
        drain(&mut rx);

        session.reconcile(&state, &joined).await;

        match rx
            .try_recv()
            .expect("the session is told to reconnect")
            .message
        {
            Some(signal_response::Message::Leave(request)) => {
                assert!(request.can_reconnect);
                assert_eq!(
                    request.action,
                    livekit::leave_request::Action::Reconnect as i32
                );
                assert_eq!(
                    request.reason,
                    livekit::DisconnectReason::StateMismatch as i32
                );
            }
            other => panic!("expected a leave request, got {other:?}"),
        }

        assert!(rx.try_recv().is_err(), "nothing follows the leave request");
        assert!(session.pending.is_none());
    }
}

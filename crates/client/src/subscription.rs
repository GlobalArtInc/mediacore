use mediacore_proto::livekit::{self, signal_request};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubscriptionRequest {
    subscribe: bool,
    track_sids: Vec<String>,
    participant_tracks: Vec<(String, Vec<String>)>,
}

impl SubscriptionRequest {
    pub fn subscribe<I, S>(track_sids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            subscribe: true,
            track_sids: track_sids.into_iter().map(Into::into).collect(),
            participant_tracks: Vec::new(),
        }
    }

    pub fn unsubscribe<I, S>(track_sids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            subscribe: false,
            track_sids: track_sids.into_iter().map(Into::into).collect(),
            participant_tracks: Vec::new(),
        }
    }

    pub fn from_participant<I, S>(
        mut self,
        participant_sid: impl Into<String>,
        track_sids: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let track_sids: Vec<String> = track_sids.into_iter().map(Into::into).collect();
        for sid in &track_sids {
            if !self.track_sids.contains(sid) {
                self.track_sids.push(sid.clone());
            }
        }
        self.participant_tracks
            .push((participant_sid.into(), track_sids));
        self
    }

    pub fn is_subscribe(&self) -> bool {
        self.subscribe
    }

    pub fn track_sids(&self) -> &[String] {
        &self.track_sids
    }

    pub fn to_proto(&self) -> livekit::UpdateSubscription {
        livekit::UpdateSubscription {
            track_sids: self.track_sids.clone(),
            subscribe: self.subscribe,
            participant_tracks: self
                .participant_tracks
                .iter()
                .map(|(participant_sid, track_sids)| livekit::ParticipantTracks {
                    participant_sid: participant_sid.clone(),
                    track_sids: track_sids.clone(),
                })
                .collect(),
        }
    }

    pub fn to_message(&self) -> signal_request::Message {
        signal_request::Message::Subscription(self.to_proto())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackSettings {
    track_sids: Vec<String>,
    disabled: bool,
    quality: Option<livekit::VideoQuality>,
    width: u32,
    height: u32,
    fps: u32,
    priority: u32,
}

impl TrackSettings {
    pub fn for_tracks<I, S>(track_sids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            track_sids: track_sids.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn with_quality(mut self, quality: livekit::VideoQuality) -> Self {
        self.quality = Some(quality);
        self
    }

    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }

    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    pub fn to_proto(&self) -> livekit::UpdateTrackSettings {
        livekit::UpdateTrackSettings {
            track_sids: self.track_sids.clone(),
            disabled: self.disabled,
            quality: self.quality.unwrap_or(livekit::VideoQuality::High) as i32,
            width: self.width,
            height: self.height,
            fps: self.fps,
            priority: self.priority,
        }
    }

    pub fn to_message(&self) -> signal_request::Message {
        signal_request::Message::TrackSetting(self.to_proto())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn encoded(message: signal_request::Message) -> signal_request::Message {
        let frame = livekit::SignalRequest {
            message: Some(message),
        };
        let bytes = frame.encode_to_vec();
        livekit::SignalRequest::decode(bytes.as_slice())
            .expect("a decodable request")
            .message
            .expect("a message")
    }

    #[test]
    fn subscribing_asks_for_the_named_tracks() {
        let request = SubscriptionRequest::subscribe(["TR_a", "TR_b"]);
        let proto = request.to_proto();

        assert!(proto.subscribe);
        assert_eq!(proto.track_sids, vec!["TR_a", "TR_b"]);
        assert!(proto.participant_tracks.is_empty());
    }

    #[test]
    fn unsubscribing_clears_the_subscribe_flag() {
        let proto = SubscriptionRequest::unsubscribe(["TR_a"]).to_proto();

        assert!(!proto.subscribe);
        assert_eq!(proto.track_sids, vec!["TR_a"]);
    }

    #[test]
    fn participant_tracks_are_mirrored_into_the_flat_sid_list() {
        let request = SubscriptionRequest::subscribe(Vec::<String>::new())
            .from_participant("PA_bob", ["TR_a", "TR_b"])
            .from_participant("PA_carol", ["TR_c"]);
        let proto = request.to_proto();

        assert_eq!(proto.track_sids, vec!["TR_a", "TR_b", "TR_c"]);
        assert_eq!(proto.participant_tracks.len(), 2);
        assert_eq!(proto.participant_tracks[0].participant_sid, "PA_bob");
        assert_eq!(proto.participant_tracks[0].track_sids, vec!["TR_a", "TR_b"]);
        assert_eq!(proto.participant_tracks[1].track_sids, vec!["TR_c"]);
    }

    #[test]
    fn a_sid_named_twice_is_listed_once() {
        let proto = SubscriptionRequest::subscribe(["TR_a"])
            .from_participant("PA_bob", ["TR_a"])
            .to_proto();

        assert_eq!(proto.track_sids, vec!["TR_a"]);
        assert_eq!(proto.participant_tracks[0].track_sids, vec!["TR_a"]);
    }

    #[test]
    fn a_subscription_survives_a_protobuf_round_trip() {
        let message = SubscriptionRequest::subscribe(["TR_a"])
            .from_participant("PA_bob", ["TR_b"])
            .to_message();

        match encoded(message) {
            signal_request::Message::Subscription(update) => {
                assert!(update.subscribe);
                assert_eq!(update.track_sids, vec!["TR_a", "TR_b"]);
                assert_eq!(update.participant_tracks[0].participant_sid, "PA_bob");
            }
            other => panic!("expected a subscription request, got {other:?}"),
        }
    }

    #[test]
    fn track_settings_ask_for_the_top_quality_unless_told_otherwise() {
        let settings = TrackSettings::for_tracks(["TR_a"]);
        assert_eq!(
            settings.to_proto().quality,
            livekit::VideoQuality::High as i32
        );

        let low = settings.with_quality(livekit::VideoQuality::Low);
        assert_eq!(low.to_proto().quality, livekit::VideoQuality::Low as i32);
    }

    #[test]
    fn asking_for_a_quality_and_asking_for_a_size_are_both_carried() {
        let message = TrackSettings::for_tracks(["TR_a"])
            .with_quality(livekit::VideoQuality::Medium)
            .with_size(640, 360)
            .to_message();

        match encoded(message) {
            signal_request::Message::TrackSetting(settings) => {
                assert_eq!(settings.quality, livekit::VideoQuality::Medium as i32);
                assert_eq!(settings.width, 640);
                assert_eq!(settings.height, 360);
            }
            other => panic!("expected track settings, got {other:?}"),
        }
    }

    #[test]
    fn track_settings_carry_the_requested_size() {
        let message = TrackSettings::for_tracks(["TR_a"])
            .with_size(320, 240)
            .with_fps(15)
            .with_priority(1)
            .disabled(true)
            .to_message();

        match encoded(message) {
            signal_request::Message::TrackSetting(settings) => {
                assert_eq!(settings.track_sids, vec!["TR_a"]);
                assert!(settings.disabled);
                assert_eq!(settings.width, 320);
                assert_eq!(settings.height, 240);
                assert_eq!(settings.fps, 15);
                assert_eq!(settings.priority, 1);
            }
            other => panic!("expected track settings, got {other:?}"),
        }
    }
}

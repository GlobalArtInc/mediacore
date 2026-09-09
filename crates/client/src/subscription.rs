use mediacore_proto::mediacore::{self, signal_request};

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

    pub fn to_proto(&self) -> mediacore::UpdateSubscription {
        mediacore::UpdateSubscription {
            track_sids: self.track_sids.clone(),
            subscribe: self.subscribe,
            participant_tracks: self
                .participant_tracks
                .iter()
                .map(
                    |(participant_sid, track_sids)| mediacore::ParticipantTracks {
                        participant_sid: participant_sid.clone(),
                        track_sids: track_sids.clone(),
                    },
                )
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
    quality: Option<mediacore::VideoQuality>,
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

    pub fn with_quality(mut self, quality: mediacore::VideoQuality) -> Self {
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

    pub fn to_proto(&self) -> mediacore::UpdateTrackSettings {
        mediacore::UpdateTrackSettings {
            track_sids: self.track_sids.clone(),
            disabled: self.disabled,
            quality: self.quality.unwrap_or(mediacore::VideoQuality::High) as i32,
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
#[path = "subscription_tests.rs"]
mod tests;

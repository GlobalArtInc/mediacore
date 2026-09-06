use std::time::Duration;

use mediacore_proto::mediacore;
use tokio::sync::broadcast;

use crate::error::ClientError;

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTrack {
    pub participant_sid: String,
    pub participant_identity: String,
    pub info: mediacore::TrackInfo,
}

impl RemoteTrack {
    pub fn sid(&self) -> &str {
        &self.info.sid
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn track_type(&self) -> mediacore::TrackType {
        mediacore::TrackType::try_from(self.info.r#type).unwrap_or(mediacore::TrackType::Audio)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    ParticipantJoined {
        participant_sid: String,
        identity: String,
    },
    ParticipantLeft {
        participant_sid: String,
        identity: String,
    },
    TrackPublished(Box<RemoteTrack>),
    TrackUnpublished {
        participant_sid: String,
        track_sid: String,
    },
    LocalTrackUnpublished {
        track_sid: String,
    },
    SubscriptionResponse {
        track_sid: String,
        error: Option<mediacore::SubscriptionError>,
    },
    TrackSubscribed {
        track_sid: String,
    },
    StreamStateChanged(Vec<mediacore::StreamStateInfo>),
    SubscribedQualityChanged {
        track_sid: String,
        codecs: Vec<mediacore::SubscribedCodec>,
        qualities: Vec<mediacore::SubscribedQuality>,
    },
    SubscriptionPermissionChanged {
        participant_sid: String,
        track_sid: String,
        allowed: bool,
    },
}

pub struct EventStream {
    inner: broadcast::Receiver<ClientEvent>,
}

impl EventStream {
    pub(crate) fn new(inner: broadcast::Receiver<ClientEvent>) -> Self {
        Self { inner }
    }

    pub async fn next(&mut self, timeout: Duration) -> Result<ClientEvent, ClientError> {
        match tokio::time::timeout(timeout, self.inner.recv()).await {
            Ok(Ok(event)) => Ok(event),
            Ok(Err(broadcast::error::RecvError::Closed)) => Err(ClientError::Closed),
            Ok(Err(broadcast::error::RecvError::Lagged(missed))) => {
                Err(ClientError::Lagged(missed))
            }
            Err(_) => Err(ClientError::Timeout("a client event")),
        }
    }

    pub async fn wait_for<T, F>(
        &mut self,
        timeout: Duration,
        mut matches: F,
    ) -> Result<T, ClientError>
    where
        F: FnMut(&ClientEvent) -> Option<T>,
    {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err(ClientError::Timeout("a matching client event"));
            }

            let event = self.next(left).await?;
            if let Some(found) = matches(&event) {
                return Ok(found);
            }
        }
    }
}

pub(crate) fn subscription_error(code: i32) -> Option<mediacore::SubscriptionError> {
    match mediacore::SubscriptionError::try_from(code) {
        Ok(mediacore::SubscriptionError::SeUnknown) => None,
        Ok(error) => Some(error),
        Err(_) => Some(mediacore::SubscriptionError::SeUnknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unknown_subscription_code_reads_as_success() {
        assert_eq!(subscription_error(0), None);
    }

    #[test]
    fn a_known_subscription_code_is_reported() {
        assert_eq!(
            subscription_error(mediacore::SubscriptionError::SeTrackNotfound as i32),
            Some(mediacore::SubscriptionError::SeTrackNotfound)
        );
    }

    #[test]
    fn an_unmapped_subscription_code_falls_back_to_unknown() {
        assert_eq!(
            subscription_error(9_999),
            Some(mediacore::SubscriptionError::SeUnknown)
        );
    }

    #[test]
    fn a_remote_track_exposes_its_sid_and_type() {
        let track = RemoteTrack {
            participant_sid: "PA_bob".to_owned(),
            participant_identity: "bob".to_owned(),
            info: mediacore::TrackInfo {
                sid: "TR_a".to_owned(),
                name: "mic".to_owned(),
                r#type: mediacore::TrackType::Audio as i32,
                ..mediacore::TrackInfo::default()
            },
        };

        assert_eq!(track.sid(), "TR_a");
        assert_eq!(track.name(), "mic");
        assert_eq!(track.track_type(), mediacore::TrackType::Audio);
    }
}

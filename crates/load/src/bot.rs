use std::time::Duration;

use anyhow::{Context, Result};
use mediacore_client::proto::{TrackType, VideoQuality};
use mediacore_client::{Client, ClientOptions, PublishedTrack, TrackSpec};

use crate::summary::Snapshot;

/// What one bot puts on the wire.
#[derive(Debug, Clone, Copy)]
pub struct Media {
    pub audio: bool,
    pub video: bool,
    pub simulcast: bool,
}

impl Media {
    /// How many tracks one bot publishes under these settings.
    pub fn tracks(&self) -> usize {
        usize::from(self.audio) + usize::from(self.video)
    }
}

/// Who a bot is and how it gets in.
#[derive(Debug, Clone)]
pub struct Plan {
    pub identity: String,
    pub room: String,
    pub token: String,
}

/// One participant: a signalling session, its published tracks, its reports.
///
/// There is no task in here. The client already runs one per transport and one
/// per direction of the socket, so a bot is a handle that a scenario holds and
/// asks; adding a task per bot would only add a queue between the scenario and
/// an answer that is already being kept up to date.
pub struct Bot {
    pub identity: String,
    pub room: String,
    client: Client,
    tracks: Vec<PublishedTrack>,
}

impl Bot {
    pub async fn join(
        plan: Plan,
        url: &str,
        media: Media,
        options: ClientOptions,
        patience: Duration,
    ) -> Result<Self> {
        let client = Client::connect_with(url, &plan.token, options)
            .await
            .with_context(|| format!("{} cannot join {}", plan.identity, plan.room))?;

        let mut tracks = Vec::new();

        if media.audio {
            tracks.push(
                client
                    .publish(TrackSpec::audio("mic"))
                    .await
                    .with_context(|| format!("{} cannot publish audio", plan.identity))?,
            );
        }

        if media.video {
            let spec = match media.simulcast {
                true => TrackSpec::simulcast_video("cam"),
                false => TrackSpec::video("cam"),
            };
            tracks.push(
                client
                    .publish(spec)
                    .await
                    .with_context(|| format!("{} cannot publish video", plan.identity))?,
            );
        }

        if !tracks.is_empty() {
            client
                .wait_connected(patience)
                .await
                .with_context(|| format!("{}'s publisher transport", plan.identity))?;

            for track in &tracks {
                track.start().await?;
            }
        }

        // Nothing subscribes here. The signalling session is opened without
        // `auto_subscribe=0`, so the server offers every track in the room as it
        // appears -- which is what a browser does, and what makes a room of N
        // bots N-1 subscriptions each without any of them asking.
        //
        // Named by what the server says rather than by what the plan asked for.
        // Under a brought token this bench does not know which room the token
        // opens, and under `duplicate_identity: suffix` the identity it asked
        // for is not the one it got -- a report naming the wrong room or the
        // wrong bot would be worse than one that had to ask.
        let identity = match client.identity() {
            "" => plan.identity,
            given => given.to_owned(),
        };
        let room = client
            .join()
            .room
            .as_ref()
            .map(|room| room.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or(plan.room);

        Ok(Self {
            identity,
            room,
            client,
            tracks,
        })
    }

    /// Everything this bot has counted, as of now.
    ///
    /// Failing to answer is not an error the caller has to handle: a bot whose
    /// tasks have gone is a bot that was disconnected, and that is a reading in
    /// its own right rather than a reason to stop the run. It comes back as a
    /// snapshot with whatever was still reachable and a disconnect reason.
    pub async fn snapshot(&self) -> Snapshot {
        let mut published = Vec::new();
        for track in &self.tracks {
            if let Ok(stats) = track.stats().await {
                published.push(stats);
            }
        }

        let received = self.client.received().await.unwrap_or_default();
        let disconnect = self
            .client
            .disconnect_reason()
            .map(|reason| reason.as_str_name().to_owned())
            .or_else(|| (published.len() < self.tracks.len()).then(|| "UNREACHABLE".to_owned()));

        Snapshot {
            identity: self.identity.clone(),
            room: self.room.clone(),
            published,
            received,
            disconnect,
        }
    }

    /// Asks for every video track it receives at `quality`.
    ///
    /// This is how a run makes the forwarder move somebody between simulcast
    /// layers on purpose. Without it a bench measures a picture that never
    /// changes: with bandwidth estimation off, nothing else in a room of bots
    /// ever asks for a different rung, so the layer-switch latency the reports
    /// can now measure would have nothing to measure.
    ///
    /// Sent and not waited on. The answer is the packets arriving on the other
    /// layer, which is exactly what the receiver is already timing.
    pub fn ask_quality(&self, quality: VideoQuality) {
        let wanted: Vec<String> = self
            .client
            .remote_tracks()
            .iter()
            .filter(|track| track.track_type() == TrackType::Video)
            .map(|track| track.sid().to_owned())
            .collect();

        if !wanted.is_empty() {
            let _ = self.client.request_quality(wanted, quality);
        }
    }

    /// Says goodbye properly, so the room ends rather than timing out.
    ///
    /// A failure is swallowed: this runs at the end of a bench, often over a
    /// server that has just been pushed past what it can do, and a bot that
    /// cannot say goodbye has nothing left to tell anybody.
    pub async fn leave(self) {
        let _ = self.client.leave().await;
    }
}

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use str0m::change::SdpPendingOffer;
use str0m::media::{Direction, MediaKind, Mid, Simulcast};
use tokio::sync::oneshot;

use crate::error::TransportError;

pub const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(15);
pub const NEGOTIATION_BACKLOG: usize = 16;

#[derive(Debug, Clone)]
pub struct MediaSpec {
    pub kind: MediaKind,
    pub direction: Direction,
    pub stream_id: Option<String>,
    pub track_id: Option<String>,
    pub simulcast: Option<Simulcast>,
}

#[derive(Debug, Clone, Default)]
pub struct Negotiation {
    pub add_media: Vec<MediaSpec>,
    pub set_direction: Vec<(Mid, Direction)>,
    pub stop_media: Vec<Mid>,
}

impl Negotiation {
    pub fn add(spec: MediaSpec) -> Self {
        Self {
            add_media: vec![spec],
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocalOffer {
    pub mids: Vec<Mid>,
    pub sdp: String,
}

pub(crate) type NegotiationReply = oneshot::Sender<Result<LocalOffer, TransportError>>;

pub(crate) struct Queued {
    pub request: Box<Negotiation>,
    pub reply: NegotiationReply,
}

struct Pending {
    offer: SdpPendingOffer,
    deadline: Option<Instant>,
}

pub(crate) struct Renegotiation {
    timeout: Duration,
    backlog: usize,
    pending: Option<Pending>,
    queued: VecDeque<Queued>,
    failed: bool,
}

impl Renegotiation {
    pub fn new(timeout: Duration, backlog: usize) -> Self {
        Self {
            timeout,
            backlog: backlog.max(1),
            pending: None,
            queued: VecDeque::new(),
            failed: false,
        }
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn has_failed(&self) -> bool {
        self.failed
    }

    pub fn queued(&self) -> usize {
        self.queued.len()
    }

    pub fn submit(&mut self, request: Box<Negotiation>, reply: NegotiationReply) {
        if self.failed {
            tracing::debug!("refusing a renegotiation, an earlier offer was never answered");
            let _ = reply.send(Err(TransportError::NegotiationFailed));
            return;
        }

        if self.queued() >= self.backlog {
            tracing::warn!(
                backlog = self.backlog,
                "refusing a renegotiation, the queue is full"
            );
            let _ = reply.send(Err(TransportError::NegotiationBacklog));
            return;
        }

        if self.is_pending() {
            tracing::debug!(
                queued = self.queued() + 1,
                "a local offer is already outstanding, queueing the renegotiation"
            );
        }

        self.queued.push_back(Queued { request, reply });
    }

    pub fn next(&mut self) -> Option<Queued> {
        if self.failed || self.is_pending() {
            return None;
        }

        self.queued.pop_front()
    }

    pub fn hold(&mut self, offer: SdpPendingOffer, now: Instant) {
        self.pending = Some(Pending {
            offer,
            deadline: Some(now + self.timeout),
        });
    }

    pub fn take(&mut self) -> Option<SdpPendingOffer> {
        let pending = self.pending.take()?;
        self.failed = false;

        Some(pending.offer)
    }

    pub fn abandon(&mut self, reason: &'static str) -> bool {
        let abandoned = self.pending.take().is_some();
        self.failed = false;

        if abandoned {
            tracing::warn!(
                reason,
                "dropping the local offer that was awaiting an answer"
            );
        }

        abandoned
    }

    pub fn fail(&mut self, reason: &'static str) -> bool {
        if self.failed {
            return false;
        }

        let Some(pending) = self.pending.as_mut() else {
            return false;
        };

        pending.deadline = None;
        self.failed = true;

        tracing::warn!(
            reason,
            queued = self.queued.len(),
            "the local offer was never answered, holding it and refusing to renegotiate"
        );

        for Queued { reply, .. } in self.queued.drain(..) {
            let _ = reply.send(Err(TransportError::NegotiationFailed));
        }

        true
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().and_then(|pending| pending.deadline)
    }

    pub fn expired(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| deadline <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Box<Negotiation> {
        Box::new(Negotiation::add(MediaSpec {
            kind: MediaKind::Video,
            direction: Direction::SendOnly,
            stream_id: None,
            track_id: None,
            simulcast: None,
        }))
    }

    #[test]
    fn a_negotiation_runs_immediately_when_nothing_is_outstanding() {
        let mut renegotiation = Renegotiation::new(NEGOTIATION_TIMEOUT, NEGOTIATION_BACKLOG);
        let (reply, _response) = oneshot::channel();

        renegotiation.submit(request(), reply);

        assert!(renegotiation.next().is_some());
        assert_eq!(renegotiation.queued(), 0);
    }

    #[tokio::test]
    async fn a_full_backlog_is_refused_with_an_error() {
        let mut renegotiation = Renegotiation::new(NEGOTIATION_TIMEOUT, 2);
        let mut responses = Vec::new();

        for _ in 0..3 {
            let (reply, response) = oneshot::channel();
            renegotiation.submit(request(), reply);
            responses.push(response);
        }

        assert_eq!(renegotiation.queued(), 2);

        let refused = responses.pop().expect("the third response");
        assert!(matches!(
            refused.await,
            Ok(Err(TransportError::NegotiationBacklog))
        ));
    }

    #[test]
    fn nothing_expires_while_no_offer_is_outstanding() {
        let mut renegotiation = Renegotiation::new(NEGOTIATION_TIMEOUT, NEGOTIATION_BACKLOG);
        let start = Instant::now();

        let (reply, _response) = oneshot::channel();
        renegotiation.submit(request(), reply);
        let _running = renegotiation.next().expect("the negotiation runs");

        assert_eq!(renegotiation.deadline(), None);
        assert!(!renegotiation.expired(start + Duration::from_secs(3600)));
        assert!(!renegotiation.abandon("nothing to abandon"));
    }
}

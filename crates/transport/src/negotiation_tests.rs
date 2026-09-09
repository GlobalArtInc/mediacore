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

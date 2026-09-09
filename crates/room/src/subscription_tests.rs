
use super::*;

fn sids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn diff_is_desired_minus_active_in_both_directions() {
    let mut subscriptions = Subscriptions::default();
    subscriptions.set_desired(sids(&["TR_a", "TR_b"]));
    subscriptions.mark_active("TR_b");
    subscriptions.mark_active("TR_c");

    let diff = subscriptions.diff();
    assert_eq!(diff.to_subscribe, sids(&["TR_a"]));
    assert_eq!(diff.to_unsubscribe, sids(&["TR_c"]));
}

#[test]
fn a_reconciled_set_has_an_empty_diff() {
    let mut subscriptions = Subscriptions::default();
    subscriptions.set_desired(sids(&["TR_a", "TR_b"]));
    for track_sid in subscriptions.diff().to_subscribe {
        subscriptions.mark_active(&track_sid);
    }

    assert!(subscriptions.diff().is_empty());
    assert_eq!(subscriptions.active(), sids(&["TR_a", "TR_b"]));
}

#[test]
fn dropping_intent_leaves_the_active_track_to_be_torn_down() {
    let mut subscriptions = Subscriptions::default();
    subscriptions.set_desired(sids(&["TR_a"]));
    subscriptions.mark_active("TR_a");
    subscriptions.remove_desired(sids(&["TR_a"]));

    assert_eq!(subscriptions.diff().to_unsubscribe, sids(&["TR_a"]));
    assert!(!subscriptions.is_desired("TR_a"));
    assert!(subscriptions.is_active("TR_a"));
}

#[test]
fn a_refusal_outlives_the_intent_it_dropped() {
    let mut subscriptions = Subscriptions::default();
    subscriptions.add_desired(sids(&["TR_a"]));
    subscriptions.decline(sids(&["TR_a"]));

    assert!(
        subscriptions.is_declined("TR_a"),
        "an unsubscribe that is not remembered is undone by the next auto-subscribe pass"
    );
    assert!(!subscriptions.is_desired("TR_a"));

    subscriptions.add_desired(sids(&["TR_a"]));
    assert!(
        !subscriptions.is_declined("TR_a"),
        "asking for a track again is how somebody takes a refusal back"
    );

    subscriptions.decline(sids(&["TR_a"]));
    subscriptions.forget(sids(&["TR_a"]));
    assert!(
        !subscriptions.is_declined("TR_a"),
        "a track that left the room takes the refusal with it, or a publisher who \
             republishes is never heard by whoever said no to the earlier track"
    );
}

#[test]
fn set_desired_replaces_the_whole_intent() {
    let mut subscriptions = Subscriptions::default();
    subscriptions.add_desired(sids(&["TR_a", "TR_b"]));
    subscriptions.set_desired(sids(&["TR_c"]));

    assert_eq!(subscriptions.desired(), sids(&["TR_c"]));
}

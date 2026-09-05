use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubscriptionDiff {
    pub to_subscribe: Vec<String>,
    pub to_unsubscribe: Vec<String>,
}

impl SubscriptionDiff {
    pub fn is_empty(&self) -> bool {
        self.to_subscribe.is_empty() && self.to_unsubscribe.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Subscriptions {
    desired: BTreeSet<String>,
    active: BTreeSet<String>,
}

impl Subscriptions {
    pub fn set_desired<I>(&mut self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.desired = track_sids.into_iter().collect();
    }

    pub fn add_desired<I>(&mut self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.desired.extend(track_sids);
    }

    pub fn remove_desired<I>(&mut self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        for track_sid in track_sids {
            self.desired.remove(&track_sid);
        }
    }

    pub fn mark_active(&mut self, track_sid: &str) {
        self.active.insert(track_sid.to_owned());
    }

    pub fn mark_inactive(&mut self, track_sid: &str) {
        self.active.remove(track_sid);
    }

    pub fn is_desired(&self, track_sid: &str) -> bool {
        self.desired.contains(track_sid)
    }

    pub fn is_active(&self, track_sid: &str) -> bool {
        self.active.contains(track_sid)
    }

    pub fn desired(&self) -> Vec<String> {
        self.desired.iter().cloned().collect()
    }

    pub fn active(&self) -> Vec<String> {
        self.active.iter().cloned().collect()
    }

    pub fn diff(&self) -> SubscriptionDiff {
        SubscriptionDiff {
            to_subscribe: self.desired.difference(&self.active).cloned().collect(),
            to_unsubscribe: self.active.difference(&self.desired).cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
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
    fn set_desired_replaces_the_whole_intent() {
        let mut subscriptions = Subscriptions::default();
        subscriptions.add_desired(sids(&["TR_a", "TR_b"]));
        subscriptions.set_desired(sids(&["TR_c"]));

        assert_eq!(subscriptions.desired(), sids(&["TR_c"]));
    }
}

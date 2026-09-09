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
    declined: BTreeSet<String>,
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
        for track_sid in track_sids {
            self.declined.remove(&track_sid);
            self.desired.insert(track_sid);
        }
    }

    pub fn remove_desired<I>(&mut self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        for track_sid in track_sids {
            self.desired.remove(&track_sid);
        }
    }

    /// Drops the intent and remembers that it was the participant's own answer.
    ///
    /// The difference from `remove_desired` is who said so. A track removed
    /// because it left the room can come back; a track somebody unsubscribed
    /// from stays unsubscribed until they ask for it again, whatever an
    /// auto-subscribe pass thinks it has not offered yet.
    pub fn decline<I>(&mut self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        for track_sid in track_sids {
            self.desired.remove(&track_sid);
            self.declined.insert(track_sid);
        }
    }

    /// Forgets a track entirely -- neither wanted nor refused.
    pub fn forget<I>(&mut self, track_sids: I)
    where
        I: IntoIterator<Item = String>,
    {
        for track_sid in track_sids {
            self.desired.remove(&track_sid);
            self.declined.remove(&track_sid);
        }
    }

    pub fn is_declined(&self, track_sid: &str) -> bool {
        self.declined.contains(track_sid)
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
#[path = "subscription_tests.rs"]
mod tests;

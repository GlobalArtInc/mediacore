use std::collections::BTreeSet;

use mediacore_proto::mediacore;

/// Who a publisher lets listen, if they have said anything about it at all.
///
/// `None` is a publisher who has never spoken, and a publisher who said
/// `all_participants`, which is the same thing: everybody the other gates
/// already allow. Naming anybody closes the audience to exactly the people
/// named.
#[derive(Debug, Default)]
pub struct Audience {
    named: Option<Vec<Listener>>,
}

#[derive(Debug)]
struct Listener {
    sid: String,
    identity: String,
    all_tracks: bool,
    track_sids: BTreeSet<String>,
}

impl Audience {
    pub(crate) fn set(&mut self, permission: &mediacore::SubscriptionPermission) {
        self.named = (!permission.all_participants).then(|| {
            permission
                .track_permissions
                .iter()
                .map(|permission| Listener {
                    sid: permission.participant_sid.clone(),
                    identity: permission.participant_identity.clone(),
                    all_tracks: permission.all_tracks,
                    track_sids: permission.track_sids.iter().cloned().collect(),
                })
                .collect()
        });
    }

    pub(crate) fn admits(&self, sid: &str, identity: &str, track_sid: &str) -> bool {
        let Some(named) = self.named.as_ref() else {
            return true;
        };

        named
            .iter()
            .any(|listener| listener.names(sid, identity) && listener.covers(track_sid))
    }

    pub fn is_open(&self) -> bool {
        self.named.is_none()
    }
}

impl Listener {
    /// Whether this entry names the participant asking to listen.
    ///
    /// The identity is matched exactly, against the identity the participant
    /// actually carries. [`Room::participants_of`](crate::Room::participants_of)
    /// deliberately reads a name as a person and answers with every connection
    /// they have; a gate has to close rather than open, and `bob#7` here must
    /// mean the session called `bob#7` rather than bob's seventh tab. An entry
    /// naming nobody admits nobody.
    fn names(&self, sid: &str, identity: &str) -> bool {
        (!self.sid.is_empty() && self.sid == sid)
            || (!self.identity.is_empty() && self.identity == identity)
    }

    fn covers(&self, track_sid: &str) -> bool {
        self.all_tracks || self.track_sids.contains(track_sid)
    }
}

#[cfg(test)]
#[path = "audience_tests.rs"]
mod tests;

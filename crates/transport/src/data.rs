use std::sync::Arc;

use str0m::channel::ChannelId;

pub const DATA_QUEUE_CAPACITY: usize = 256;
pub const MAX_DATA_PACKET: usize = 64 * 1024;
pub const RELIABLE_LABEL: &str = "_reliable";
pub const LOSSY_LABEL: &str = "_lossy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataChannel {
    Reliable,
    Lossy,
}

impl DataChannel {
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            RELIABLE_LABEL => Some(Self::Reliable),
            LOSSY_LABEL => Some(Self::Lossy),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Reliable => RELIABLE_LABEL,
            Self::Lossy => LOSSY_LABEL,
        }
    }

    pub fn is_reliable(self) -> bool {
        matches!(self, Self::Reliable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DataSettings {
    pub max_packet: usize,
}

impl Default for DataSettings {
    fn default() -> Self {
        Self {
            max_packet: MAX_DATA_PACKET,
        }
    }
}

#[derive(Debug)]
pub struct IncomingData {
    pub channel: DataChannel,
    pub binary: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct OutgoingData {
    pub channel: DataChannel,
    pub binary: bool,
    pub payload: Arc<[u8]>,
}

#[derive(Debug, Default)]
pub struct DataChannels {
    open: Vec<(ChannelId, DataChannel)>,
}

impl DataChannels {
    pub fn opened(&mut self, id: ChannelId, label: &str) -> Option<DataChannel> {
        let channel = DataChannel::from_label(label)?;

        self.open
            .retain(|(known, kind)| *known != id && *kind != channel);
        self.open.push((id, channel));

        Some(channel)
    }

    pub fn closed(&mut self, id: ChannelId) -> Option<DataChannel> {
        let index = self.open.iter().position(|(known, _)| *known == id)?;

        Some(self.open.remove(index).1)
    }

    pub fn kind(&self, id: ChannelId) -> Option<DataChannel> {
        self.open
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, kind)| *kind)
    }

    pub fn id(&self, channel: DataChannel) -> Option<ChannelId> {
        self.open
            .iter()
            .find(|(_, kind)| *kind == channel)
            .map(|(id, _)| *id)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use str0m::Rtc;
    use str0m::channel::ChannelConfig;

    use super::*;

    fn channel_ids(count: usize) -> Vec<ChannelId> {
        let mut rtc = Rtc::builder().build(Instant::now());
        let mut api = rtc.direct_api();

        (0..count)
            .map(|index| {
                api.create_data_channel(ChannelConfig {
                    label: format!("channel-{index}"),
                    ..ChannelConfig::default()
                })
            })
            .collect()
    }

    #[test]
    fn a_label_this_server_does_not_carry_is_not_taken() {
        let ids = channel_ids(1);
        let mut channels = DataChannels::default();

        assert_eq!(channels.opened(ids[0], "_data_track"), None);
        assert_eq!(channels.kind(ids[0]), None);
        assert_eq!(channels.id(DataChannel::Reliable), None);
    }

    #[test]
    fn both_labels_are_carried_side_by_side() {
        let ids = channel_ids(2);
        let mut channels = DataChannels::default();

        assert_eq!(
            channels.opened(ids[0], RELIABLE_LABEL),
            Some(DataChannel::Reliable)
        );
        assert_eq!(
            channels.opened(ids[1], LOSSY_LABEL),
            Some(DataChannel::Lossy)
        );

        assert_eq!(channels.id(DataChannel::Reliable), Some(ids[0]));
        assert_eq!(channels.id(DataChannel::Lossy), Some(ids[1]));
        assert_eq!(channels.kind(ids[1]), Some(DataChannel::Lossy));
    }

    #[test]
    fn reopening_a_label_leaves_the_previous_channel_behind() {
        let ids = channel_ids(2);
        let mut channels = DataChannels::default();

        channels.opened(ids[0], RELIABLE_LABEL);
        channels.opened(ids[1], RELIABLE_LABEL);

        assert_eq!(channels.id(DataChannel::Reliable), Some(ids[1]));
        assert_eq!(channels.kind(ids[0]), None);
    }

    #[test]
    fn closing_forgets_only_the_channel_named() {
        let ids = channel_ids(2);
        let mut channels = DataChannels::default();

        channels.opened(ids[0], RELIABLE_LABEL);
        channels.opened(ids[1], LOSSY_LABEL);

        assert_eq!(channels.closed(ids[0]), Some(DataChannel::Reliable));
        assert_eq!(channels.closed(ids[0]), None);
        assert_eq!(channels.id(DataChannel::Reliable), None);
        assert_eq!(channels.id(DataChannel::Lossy), Some(ids[1]));
    }
}

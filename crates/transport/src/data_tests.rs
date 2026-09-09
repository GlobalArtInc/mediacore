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

use super::*;

#[test]
fn the_rtx_cache_is_sized_per_media_kind() {
    let audio = RtxCache::for_kind(MediaKind::Audio);
    let video = RtxCache::for_kind(MediaKind::Video);

    assert_eq!(audio.packets, AUDIO_RTX_CACHE_PACKETS);
    assert_eq!(video.packets, VIDEO_RTX_CACHE_PACKETS);
    assert!(
        video.packets > audio.packets,
        "video needs a deeper resend window than audio"
    );
    assert_eq!(audio.age, RTX_CACHE_AGE);
    assert_eq!(video.ratio_cap, RTX_RATIO_CAP);
}

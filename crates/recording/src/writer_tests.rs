
use super::*;

fn writing(directory: &std::path::Path) -> (Writing, Arc<AtomicU64>) {
    let abandoned = Arc::new(AtomicU64::new(0));
    let (finished, _taken) = tokio::sync::mpsc::unbounded_channel();

    let writing = Writing::new(
        Settings {
            node: "ND_one".to_owned(),
            directory: directory.to_path_buf(),
            max_segment_bytes: 64 * 1024 * 1024,
            reorder_hold: Duration::from_millis(5),
            queue: 64,
        },
        finished,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        Arc::clone(&abandoned),
    );

    (writing, abandoned)
}

/// A panic on this thread costs the track it happened in and nothing else.
///
/// **This thread is the one every recording on the process shares**, and
/// what reaches it came off the network from somebody else's encoder. It
/// used to be that a panic here killed the thread, every later
/// `Tap::packet` hit a closed channel, and the node reported
/// `recording=on dropped_audio_packets=0` while recording nothing at all
/// for the rest of its life. The rule now is that one packet costs at most
/// the recording it belongs to, and says so.
///
/// This deliberately panics inside the shield, so the test prints a panic
/// message and a note about `RUST_BACKTRACE` and then passes. That output
/// is the default panic hook doing its job -- the point of the shield is
/// that the process carries on afterwards, not that the panic goes unsaid.
#[test]
fn a_panic_in_the_writer_costs_one_track_and_leaves_the_thread_running() {
    let directory = tempfile::TempDir::new().expect("a directory");
    let (mut writing, abandoned) = writing(directory.path());

    for key in [1u64, 2] {
        writing.handle(Message::Open {
            key,
            spec: Box::new(TrackSpec {
                room: "standup".to_owned(),
                participant: format!("PA_0000000000{key}"),
                identity: "alice".to_owned(),
                track: "TR_audio".to_owned(),
            }),
        });
    }

    writing.shielded(Some(1), |_| panic!("a packet nobody expected"));

    assert_eq!(
        abandoned.load(Ordering::Relaxed),
        1,
        "a recording the writer gave up on has to be counted, or a node that has stopped \
             recording somebody's audio looks exactly like one that never had any to record"
    );
    assert!(
        !writing.tracks.contains_key(&1),
        "the track that panicked has to be dropped: whatever state it was left in is the \
             state that panics again on its next packet, once per tick, for ever"
    );
    assert!(
        writing.tracks.contains_key(&2),
        "and every other recording on the node has to survive it -- which is the whole \
             difference between one lost recording and a node that silently stops recording"
    );

    // And the thread goes on working: another track opens as if nothing had
    // happened.
    writing.handle(Message::Open {
        key: 3,
        spec: Box::new(TrackSpec {
            room: "standup".to_owned(),
            participant: "PA_0000000000003".to_owned(),
            identity: "carol".to_owned(),
            track: "TR_audio".to_owned(),
        }),
    });
    assert!(writing.tracks.contains_key(&3));
}

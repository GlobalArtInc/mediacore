use super::*;

/// A recorder's queue with nothing at the other end of it.
///
/// The receiver is held here and never read, which is a writer thread that
/// has stopped dead -- a disk that has wedged, a filesystem that has gone
/// read-only and is taking its time about saying so. It is the state the
/// bound exists for, and the one a bounded channel would turn into a media
/// path that blocks.
fn wedged(limit: usize) -> (Tap, std::sync::mpsc::Receiver<Message>) {
    let (messages, inbox) = channel();
    let shared = Arc::new(Shared {
        messages,
        queued: Arc::new(AtomicUsize::new(0)),
        limit,
        dropped: AtomicU64::new(0),
        next_key: AtomicU64::new(1),
        alive: Arc::new(AtomicBool::new(true)),
        abandoned: Arc::new(AtomicU64::new(0)),
    });

    (Tap { shared, key: 1 }, inbox)
}

#[test]
fn a_wedged_writer_costs_audio_and_never_a_moment_of_anybody_carrying_a_call() {
    let (tap, held) = wedged(64);
    let payload: Arc<[u8]> = Arc::from(vec![0b0111_1000u8; 80].into_boxed_slice());

    // Every one of these has to return. The test finishing at all is the
    // assertion: a bounded channel here, or a lock shared with the writer,
    // would stop on the sixty-fifth and never come back.
    for sequence in 0..1_064u64 {
        tap.packet(
            sequence,
            sequence * 960,
            48_000,
            Arc::clone(&payload),
            Instant::now(),
        );
    }

    assert_eq!(
        tap.shared.dropped.load(Ordering::Relaxed),
        1_000,
        "everything past the queue's depth has to be dropped and counted; what is lost shows \
             up in the file as a gap of the right length, because the clock is the sender's"
    );
    assert_eq!(
        tap.shared.queued.load(Ordering::Relaxed),
        64,
        "and the queue must not grow past its bound, or a disk that has stopped becomes a \
             process that is killed for memory"
    );
    drop(held);
}

/// A writer that has gone costs audio, and the audio it costs is counted.
///
/// The counting is the half worth guarding. A recorder whose thread has
/// stopped goes on accepting packets -- it must, because the media path
/// hands them over and nothing there may wait or fail -- so the only thing
/// that says the node has stopped recording is this number and
/// `Recorder::alive`. Without the count, `/healthz` reports
/// `dropped_audio_packets=0` for the rest of the process's life while not
/// one byte is being written.
#[test]
fn a_recorder_whose_thread_is_gone_counts_every_packet_it_can_no_longer_write() {
    let (tap, held) = wedged(8);
    drop(held);

    for sequence in 0..5u64 {
        tap.packet(
            sequence,
            sequence * 960,
            48_000,
            Arc::from(vec![0u8; 4].into_boxed_slice()),
            Instant::now(),
        );
    }

    assert_eq!(
        tap.shared.queued.load(Ordering::Relaxed),
        0,
        "a send that failed has to give its place in the queue back, or a writer that has \
             stopped leaves the queue permanently full and every later packet counted as dropped \
             twice"
    );
    assert_eq!(
        tap.shared.dropped.load(Ordering::Relaxed),
        5,
        "audio that reached a writer which is no longer there was thrown away and not \
             counted, so a node that has silently stopped recording reports \
             dropped_audio_packets=0 and looks exactly like one that is working"
    );
}

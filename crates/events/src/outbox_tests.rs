
use super::*;
use mediacore_proto::mediacore;

fn directory() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("a directory for the outbox")
}

fn event(name: &str) -> Arc<Event> {
    Arc::new(Event::room_started(mediacore::Room {
        sid: "RM_000000000000".to_owned(),
        name: name.to_owned(),
        ..Default::default()
    }))
}

fn names(of: &[&str]) -> Vec<String> {
    of.iter().map(|name| (*name).to_owned()).collect()
}

/// The failure this module exists to prevent, as a test: a destination
/// that was away when the event happened and a process that stopped while
/// it was still away.
#[test]
fn an_event_nobody_took_is_still_owed_after_a_restart() {
    let directory = directory();
    let promised = event("standup");

    {
        let outbox = Outbox::open(directory.path()).expect("an empty outbox opens");
        outbox.record(&promised, &names(&["webhook"]));
        assert_eq!(outbox.owed(), 1);
    }

    let outbox = Outbox::open(directory.path()).expect("the outbox reopens");
    let restored = outbox.restore(&names(&["webhook"]));

    assert_eq!(
        restored.len(),
        1,
        "an event promised to a destination that was down was forgotten when the process \
             stopped, which is the outage nobody ever finds out about"
    );
    assert_eq!(restored[0].event.id, promised.id);
    assert_eq!(restored[0].event.room_name(), "standup");
    assert_eq!(restored[0].pending, names(&["webhook"]));
}

#[test]
fn an_event_everybody_took_is_gone() {
    let directory = directory();
    let outbox = Outbox::open(directory.path()).unwrap();

    let seq = outbox.record(&event("standup"), &names(&["webhook", "kafka"]));
    outbox.settle(seq, "webhook");
    assert_eq!(
        outbox.owed(),
        1,
        "an event was struck off while a destination still owed it"
    );

    outbox.settle(seq, "kafka");
    assert_eq!(outbox.owed(), 0);

    let reopened = Outbox::open(directory.path()).unwrap();
    assert!(
        reopened.restore(&names(&["webhook", "kafka"])).is_empty(),
        "a delivered event was left on the disk, so every restart sends it again for ever"
    );
}

/// The reason a record names its destinations one by one. A broker that
/// was down for an hour must not cause an hour of redelivery to the
/// webhook endpoint that took everything the first time.
#[test]
fn a_restart_owes_an_event_only_to_whoever_had_not_taken_it() {
    let directory = directory();

    {
        let outbox = Outbox::open(directory.path()).unwrap();
        let seq = outbox.record(&event("standup"), &names(&["webhook", "kafka"]));
        outbox.settle(seq, "webhook");
    }

    let restored = Outbox::open(directory.path())
        .unwrap()
        .restore(&names(&["webhook", "kafka"]));

    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored[0].pending,
        names(&["kafka"]),
        "the endpoint that already answered 200 is being sent the event a second time"
    );
}

#[test]
fn what_is_owed_comes_back_in_the_order_it_happened() {
    let directory = directory();

    {
        let outbox = Outbox::open(directory.path()).unwrap();
        for index in 0..12 {
            outbox.record(&event(&format!("room-{index}")), &names(&["webhook"]));
        }
    }

    let restored = Outbox::open(directory.path())
        .unwrap()
        .restore(&names(&["webhook"]));

    assert_eq!(
        restored
            .iter()
            .map(|owed| owed.event.room_name().to_owned())
            .collect::<Vec<String>>(),
        (0..12)
            .map(|index| format!("room-{index}"))
            .collect::<Vec<String>>(),
        "the directory was read back in whatever order the filesystem listed it, so a \
             participant_left can arrive before the participant_joined it followed"
    );
}

/// A restart must not start numbering again from zero on top of records
/// that are still there, or two events share a file and one of them is
/// quietly overwritten.
#[test]
fn a_restart_carries_on_numbering_where_the_last_one_stopped() {
    let directory = directory();

    {
        let outbox = Outbox::open(directory.path()).unwrap();
        outbox.record(&event("standup"), &names(&["webhook"]));
        outbox.record(&event("payroll"), &names(&["webhook"]));
    }

    let outbox = Outbox::open(directory.path()).unwrap();
    outbox.restore(&names(&["webhook"]));
    let seq = outbox.record(&event("retro"), &names(&["webhook"]));

    assert_eq!(seq, 2, "the new event was filed on top of an old one");
    assert_eq!(outbox.owed(), 3);
}

/// An operator who removes an endpoint from the config has removed it. A
/// record still naming it would be read back on every start, for ever,
/// and nothing would ever settle it.
#[test]
fn a_destination_that_no_longer_exists_is_struck_off() {
    let directory = directory();

    {
        let outbox = Outbox::open(directory.path()).unwrap();
        outbox.record(&event("standup"), &names(&["retired-endpoint"]));
    }

    {
        let outbox = Outbox::open(directory.path()).unwrap();
        assert!(outbox.restore(&names(&["webhook"])).is_empty());
    }

    let outbox = Outbox::open(directory.path()).unwrap();
    assert!(
        outbox.restore(&names(&["webhook"])).is_empty(),
        "a record owed to nobody survived a start, so it is read back on every start from now on"
    );
    assert_eq!(outbox.owed(), 0);
}

/// One unreadable file is one event nobody receives. Refusing to open the
/// outbox over it would take every other event down with it, and the
/// server with them.
#[test]
fn a_file_that_is_not_a_record_costs_one_event_and_not_the_server() {
    let directory = directory();

    {
        let outbox = Outbox::open(directory.path()).unwrap();
        outbox.record(&event("standup"), &names(&["webhook"]));
    }
    std::fs::write(
        directory.path().join("000000000009-EV_x.json"),
        b"{ not json",
    )
    .unwrap();
    std::fs::write(directory.path().join("notes.txt"), b"an operator's note").unwrap();
    std::fs::write(
        directory.path().join("000000000010-EV_y.json.writing"),
        b"{ half",
    )
    .unwrap();

    let outbox = Outbox::open(directory.path())
        .expect("one unreadable file took every other event on this server with it");

    assert_eq!(
        outbox.restore(&names(&["webhook"])).len(),
        1,
        "a file that is not a record was read back as an event"
    );
}

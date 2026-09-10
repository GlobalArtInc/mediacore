use std::collections::HashMap;

use super::*;

/// A reading with a different number in every field.
///
/// Distinct on purpose: a renderer that puts one field's value where another
/// belongs reads perfectly well when everything is zero, which is what a
/// freshly started server answers and therefore what a careless test would
/// check.
fn reading() -> Snapshot {
    Snapshot {
        rooms: 1,
        participants: 2,
        publishers: 3,
        rtp_packets: 4,
        rtp_bytes: 5,
        tracks: 6,
        subscriptions: 7,
        forwarded_packets: 8,
        forwarded_bytes: 9,
        bwe: true,
        allocated_subscribers: 10,
        store: "files:/var/lib/mediacore/sessions".to_owned(),
        store_up: true,
        sessions: 11,
        live_sessions: 12,
        resident_sessions: 13,
        owed_sessions: 14,
        unwritten_sessions: 15,
        events: 16,
        owed_events: 17,
        undelivered_events: 18,
        unwritten_events: 19,
        recorder: Recorder::On,
        owed_recordings: 20,
        dropped_audio_packets: 21,
        abandoned_recordings: 22,
        stuck_recording_work: 23,
        ice: "127.0.0.1:7882".parse().unwrap(),
        draining: false,
        node: "ND_test".to_owned(),
        version: "9.9.9",
        region: "eu-west".to_owned(),
        cluster_joined: true,
        cluster_nodes: 24,
        cluster_nodes_alive: 25,
    }
}

/// The whole line, character for character.
///
/// Pinned as one string rather than field by field because what a caller reads
/// is the string: the compose healthcheck greps it, three test files split it
/// on whitespace and then on `=`, and any of an extra space, a renamed field or
/// a reordering breaks one of them silently. A change that has to be made is
/// made here first, in one obvious diff, and then in the readers.
#[test]
fn the_health_line_is_what_it_has_always_been() {
    assert_eq!(
        reading().health_line(),
        "rooms=1 participants=2 publishers=3 rtp_packets=4 rtp_bytes=5 tracks=6 subscriptions=7 \
         forwarded_packets=8 forwarded_bytes=9 bwe=on allocated_subscribers=10 \
         session_store=files:/var/lib/mediacore/sessions:up sessions=11 live_sessions=12 \
         resident_sessions=13 owed_sessions=14 unwritten_sessions=15 events=16 owed_events=17 \
         undelivered_events=18 unwritten_events=19 recording=on unwritten_recordings=20 \
         dropped_audio_packets=21 abandoned_recordings=22 stuck_recording_work=23 \
         ice=127.0.0.1:7882"
    );
}

/// The three fields on the line that are words rather than numbers. An
/// operator's eye and at least one grep are on these exact words.
#[test]
fn a_state_on_the_line_is_the_word_it_has_always_been() {
    let mut reading = reading();
    reading.store_up = false;
    reading.bwe = false;
    reading.recorder = Recorder::Off;

    let line = reading.health_line();
    assert!(line.contains(" session_store=files:/var/lib/mediacore/sessions:down "));
    assert!(line.contains(" bwe=off "));
    assert!(line.contains(" recording=off "));

    reading.recorder = Recorder::Stopped;
    assert!(reading.health_line().contains(" recording=stopped "));
}

/// The line is one line and ends without a newline, which is how it was written
/// and how the tests that read it split it.
#[test]
fn the_health_line_is_one_line() {
    let line = reading().health_line();

    assert_eq!(line.lines().count(), 1);
    assert!(!line.ends_with('\n'));
    assert!(
        !line.contains("  "),
        "two spaces somewhere: a `\\` continuation lost its space"
    );
}

/// Every number on the line, and the series that has to carry it.
///
/// Written out by hand here so that the table is a second opinion rather than
/// the renderer repeating itself: a field added to one of the two renderings
/// and forgotten in the other fails this test, which is the whole reason the
/// two of them read one snapshot.
const SHARED: &[(&str, &str)] = &[
    ("rooms", "mediacore_rooms"),
    ("participants", "mediacore_participants"),
    ("publishers", "mediacore_publishers"),
    ("rtp_packets", "mediacore_rtp_packets_total"),
    ("rtp_bytes", "mediacore_rtp_bytes_total"),
    ("tracks", "mediacore_tracks"),
    ("subscriptions", "mediacore_subscriptions"),
    ("forwarded_packets", "mediacore_forwarded_packets_total"),
    ("forwarded_bytes", "mediacore_forwarded_bytes_total"),
    ("allocated_subscribers", "mediacore_allocated_subscribers"),
    ("sessions", "mediacore_sessions"),
    ("live_sessions", "mediacore_live_sessions"),
    ("resident_sessions", "mediacore_resident_sessions"),
    ("owed_sessions", "mediacore_owed_sessions"),
    ("unwritten_sessions", "mediacore_unwritten_sessions"),
    ("events", "mediacore_events_emitted_total"),
    ("owed_events", "mediacore_owed_events"),
    ("undelivered_events", "mediacore_undelivered_events_total"),
    ("unwritten_events", "mediacore_unwritten_events_total"),
    ("unwritten_recordings", "mediacore_owed_recordings"),
    (
        "dropped_audio_packets",
        "mediacore_dropped_audio_packets_total",
    ),
    (
        "abandoned_recordings",
        "mediacore_abandoned_recordings_total",
    ),
    ("stuck_recording_work", "mediacore_stuck_recording_work"),
];

/// The fields the two renderings say differently on purpose. `session_store`,
/// `bwe` and `recording` are words on the line and numbers in the exposition,
/// and `ice` is an address -- not a measurement, and the only place it could go
/// in the exposition is a label, which is what this document does not do.
const WORDED: &[&str] = &["session_store", "bwe", "recording", "ice"];

#[test]
fn the_two_renderings_are_one_reading() {
    let reading = reading();
    let health_line = reading.health_line();
    let line = fields(&health_line);
    let series = series(&reading.exposition());

    for (field, name) in SHARED {
        let value = line
            .get(field)
            .unwrap_or_else(|| panic!("`{field}` is not on the health line any more"));
        let carried = series.get(*name).unwrap_or_else(|| {
            panic!("`{field}` is on the health line and `{name}` is not in the exposition")
        });

        assert_eq!(
            *value,
            carried.value.to_string(),
            "`{field}` and `{name}` are two numbers for the same thing"
        );
    }

    for field in line.keys() {
        assert!(
            SHARED.iter().any(|(known, _)| known == field) || WORDED.contains(field),
            "`{field}` is on the health line and in neither table: it is a number no alert can \
             see"
        );
    }

    assert_eq!(series["mediacore_session_store_up"].value, 1);
    assert_eq!(series["mediacore_bwe_enabled"].value, 1);
    assert_eq!(series["mediacore_recorder_enabled"].value, 1);
    assert_eq!(series["mediacore_recorder_running"].value, 1);
}

/// The states the line has one word for and the exposition has two numbers
/// for. `off` and `stopped` are different failures with different remedies, so
/// they cannot both be a zero.
#[test]
fn a_recorder_that_died_is_not_a_recorder_that_was_never_asked_to_run() {
    let mut reading = reading();

    reading.recorder = Recorder::Off;
    let off = series(&reading.exposition());
    assert_eq!(off["mediacore_recorder_enabled"].value, 0);
    assert_eq!(off["mediacore_recorder_running"].value, 0);

    reading.recorder = Recorder::Stopped;
    let stopped = series(&reading.exposition());
    assert_eq!(stopped["mediacore_recorder_enabled"].value, 1);
    assert_eq!(stopped["mediacore_recorder_running"].value, 0);
}

#[test]
fn every_series_is_described_and_named_once() {
    let exposition = reading().exposition();
    let series = series(&exposition);

    assert!(!series.is_empty());

    for (name, series) in &series {
        assert!(
            name.starts_with("mediacore_"),
            "`{name}` is in somebody else's namespace"
        );
        assert!(!series.help.is_empty(), "`{name}` is described by nothing");

        match series.kind.as_str() {
            "counter" => assert!(
                name.ends_with("_total"),
                "`{name}` is a counter and does not say so in its name"
            ),
            "gauge" => assert!(
                !name.ends_with("_total"),
                "`{name}` is a gauge wearing a counter's name"
            ),
            kind => panic!("`{name}` is a {kind}, which nothing here should be emitting"),
        }
    }
}

/// The rule the whole module exists to keep. A label per room is the
/// cardinality this server cannot afford, and the room names would be in the
/// time series database for ever besides.
#[test]
fn nothing_but_the_build_info_carries_a_label() {
    let exposition = reading().exposition();

    for (name, series) in series(&exposition) {
        if name == "mediacore_build_info" {
            assert_eq!(
                series.value, 1,
                "an info series is a 1 or it joins onto nothing"
            );
            assert_eq!(
                series.labels,
                r#"node="ND_test",version="9.9.9",region="eu-west""#
            );
            continue;
        }

        assert!(
            series.labels.is_empty(),
            "`{name}` carries labels: {}",
            series.labels
        );
    }
}

/// The node id and the region come out of a configuration file somebody else
/// wrote. A quote in either would otherwise end the label set early and leave
/// the rest of the document unparseable.
#[test]
fn a_label_value_cannot_end_the_line_it_is_on() {
    let mut reading = reading();
    reading.node = r#"ND_"; DROP"#.to_owned();
    reading.region = "over\\there".to_owned();

    let exposition = reading.exposition();
    let series = series(&exposition);

    assert_eq!(
        series["mediacore_build_info"].labels,
        r#"node="ND_\"; DROP",version="9.9.9",region="over\\there""#
    );
}

/// The node most deployments run: one replica, no recorder, nothing subscribing
/// to events, and on its way out. Every one of those is a `None` or a `false`
/// somewhere in the gathering, and rendering must be the same shape as for a
/// busy node in a cluster.
#[test]
fn a_draining_node_with_nothing_switched_on_renders() {
    let bare = Snapshot {
        recorder: Recorder::Off,
        draining: true,
        cluster_joined: false,
        cluster_nodes: 0,
        cluster_nodes_alive: 0,
        store_up: false,
        bwe: false,
        region: String::new(),
        ..reading()
    };

    let exposition = bare.exposition();
    let series = series(&exposition);

    assert_eq!(series["mediacore_draining"].value, 1);
    assert_eq!(series["mediacore_cluster_joined"].value, 0);
    assert_eq!(series["mediacore_cluster_nodes"].value, 0);
    assert_eq!(series["mediacore_session_store_up"].value, 0);
    assert_eq!(series["mediacore_bwe_enabled"].value, 0);
    assert_eq!(
        series["mediacore_build_info"].labels,
        r#"node="ND_test",version="9.9.9",region="""#
    );
    assert!(bare.health_line().starts_with("rooms="));
}

/// One series as the document describes it.
#[derive(Debug)]
struct Series {
    help: String,
    kind: String,
    labels: String,
    value: u64,
}

/// The health line, split the way its readers split it.
fn fields(line: &str) -> HashMap<&str, &str> {
    line.split(' ')
        .map(|field| {
            field
                .split_once('=')
                .unwrap_or_else(|| panic!("`{field}` on the health line is not `name=value`"))
        })
        .collect()
}

/// A parser strict enough to fail the ways a collector would.
///
/// Written here rather than taken as a dependency, and not only to keep the
/// dependency out: a parser from the same crate as the writer agrees with the
/// writer about everything, including whatever they are both wrong about. This
/// one knows only what the exposition format says -- every line is a comment or
/// a sample, every series is described before it is sampled, every value is a
/// number -- and it panics on the spot rather than returning an error, because
/// it is only ever reading a document this process just wrote.
fn series(text: &str) -> HashMap<String, Series> {
    assert!(
        text.ends_with('\n'),
        "the document does not end with a newline"
    );

    let mut help: HashMap<String, String> = HashMap::new();
    let mut kind: HashMap<String, String> = HashMap::new();
    let mut series: HashMap<String, Series> = HashMap::new();

    for line in text.lines() {
        assert!(!line.trim().is_empty(), "a blank line");

        if let Some(rest) = line.strip_prefix("# HELP ") {
            let (name, text) = rest
                .split_once(' ')
                .unwrap_or_else(|| panic!("`{line}` describes nothing"));
            assert!(
                help.insert(name.to_owned(), text.to_owned()).is_none(),
                "`{name}` is described twice"
            );
            continue;
        }

        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, text) = rest
                .split_once(' ')
                .unwrap_or_else(|| panic!("`{line}` types nothing"));
            assert!(
                kind.insert(name.to_owned(), text.to_owned()).is_none(),
                "`{name}` is typed twice"
            );
            continue;
        }

        assert!(
            !line.starts_with('#'),
            "`{line}` is a comment that is neither HELP nor TYPE"
        );

        // From the right: the value is the last thing on the line, and a label
        // value in between is free to contain a space.
        let (left, value) = line
            .rsplit_once(' ')
            .unwrap_or_else(|| panic!("`{line}` is not `name value`"));
        let value: u64 = value
            .parse()
            .unwrap_or_else(|_| panic!("`{value}` is not a number"));

        let (name, labels) = match left.split_once('{') {
            Some((name, labels)) => (
                name,
                labels
                    .strip_suffix('}')
                    .unwrap_or_else(|| panic!("`{line}` never closes its labels")),
            ),
            None => (left, ""),
        };

        assert!(
            name.chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_'),
            "`{name}` is not a metric name"
        );

        let described = help
            .remove(name)
            .unwrap_or_else(|| panic!("`{name}` is sampled with no HELP before it"));
        let typed = kind
            .remove(name)
            .unwrap_or_else(|| panic!("`{name}` is sampled with no TYPE before it"));

        let already = series.insert(
            name.to_owned(),
            Series {
                help: described,
                kind: typed,
                labels: labels.to_owned(),
                value,
            },
        );
        assert!(already.is_none(), "`{name}` is sampled twice");
    }

    assert!(
        help.is_empty() && kind.is_empty(),
        "described and never sampled: {:?}",
        help.keys().chain(kind.keys()).collect::<Vec<_>>()
    );

    series
}

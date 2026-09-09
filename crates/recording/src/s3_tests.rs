
use super::*;

fn settings() -> S3Settings {
    S3Settings {
        endpoint: "https://objects.example.net:9000".to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "meetings".to_owned(),
        access_key_id: "AKIDEXAMPLE".to_owned(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
        path_style: true,
        prefix: String::new(),
    }
}

fn recording() -> Recording {
    Recording {
        id: "RE_0123456789ab".to_owned(),
        session: "SE_0123456789ab".to_owned(),
        participant: "PA_0123456789ab".to_owned(),
        segment: 2,
        local_path: PathBuf::from("/var/lib/mediacore/audio/x.ogg"),
        node: Some("ND_one".to_owned()),
        object_key: None,
        bytes: None,
        sample_rate: 48_000,
        channels: 1,
        started_at_ms: 0,
        ended_at_ms: None,
        offset_ms: 0,
        duration_ms: None,
    }
}

/// AWS's own published example -- "Signature Version 4 signing process",
/// the `ListUsers` request -- checked independently with `openssl dgst
/// -sha256 -mac HMAC` before it was written down here.
///
/// This is the only thing standing between a hand-written signature and a
/// deployment that records for a day and cannot upload any of it, so it
/// asserts the exact bytes rather than that something plausible came out.
#[test]
fn the_signature_matches_the_one_aws_publishes_for_its_own_example() {
    let header = authorization(
        "GET",
        "/",
        "Action=ListUsers&Version=2010-05-08",
        &[
            (
                "content-type".to_owned(),
                "application/x-www-form-urlencoded; charset=utf-8".to_owned(),
            ),
            ("host".to_owned(), "iam.amazonaws.com".to_owned()),
            ("x-amz-date".to_owned(), "20150830T123600Z".to_owned()),
        ],
        &hex(&Sha256::digest(b"")),
        "20150830T123600Z",
        "20150830",
        "us-east-1",
        "iam",
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
    );

    assert_eq!(
        header,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7",
        "the signature no longer matches AWS's published example, so every upload this server \
             makes would be refused with SignatureDoesNotMatch"
    );
}

#[test]
fn the_timestamp_is_the_one_the_signature_is_dated_with() {
    // 2015-08-30T12:36:00Z, the moment AWS's example is signed at.
    assert_eq!(
        utc(1_440_938_160),
        ("20150830T123600Z".to_owned(), "20150830".to_owned())
    );
    // A leap day, which is the arithmetic worth checking.
    assert_eq!(utc(1_709_208_000).1, "20240229");
    assert_eq!(
        utc(0),
        ("19700101T000000Z".to_owned(), "19700101".to_owned())
    );
}

#[test]
fn a_key_names_the_meeting_the_participant_and_which_file_it_is() {
    let store = S3::new(settings(), std::time::Duration::from_secs(5)).expect("a client");

    assert_eq!(
        store.key_for(&recording()),
        "SE_0123456789ab/PA_0123456789ab-2.ogg",
        "an object has to be findable from a row and a row from an object without asking \
             anything in between"
    );
}

#[test]
fn a_prefix_goes_in_front_and_leaves_the_rest_alone() {
    let store = S3::new(
        S3Settings {
            prefix: "/mediacore/audio/".to_owned(),
            ..settings()
        },
        std::time::Duration::from_secs(5),
    )
    .expect("a client");

    assert_eq!(
        store.key_for(&recording()),
        "mediacore/audio/SE_0123456789ab/PA_0123456789ab-2.ogg",
        "a prefix written with slashes around it must not produce an empty path segment"
    );
}

#[test]
fn path_style_and_virtual_host_style_both_reach_the_same_object() {
    let path_style = S3::new(settings(), std::time::Duration::from_secs(5)).expect("a client");
    let virtual_host = S3::new(
        S3Settings {
            path_style: false,
            ..settings()
        },
        std::time::Duration::from_secs(5),
    )
    .expect("a client");

    assert_eq!(
        path_style.object_url("a/b.ogg").as_str(),
        "https://objects.example.net:9000/meetings/a/b.ogg"
    );
    assert_eq!(
        virtual_host.object_url("a/b.ogg").as_str(),
        "https://meetings.objects.example.net:9000/a/b.ogg",
        "Amazon's own S3 wants the bucket in the host for anything made since 2020, and a \
             deployment that cannot ask for that cannot use S3"
    );
}

#[test]
fn the_signed_host_is_the_one_the_request_will_send() {
    let explicit = Url::parse("https://objects.example.net:9000/x").expect("a url");
    let default = Url::parse("https://objects.example.net/x").expect("a url");

    assert_eq!(authority(&explicit), "objects.example.net:9000");
    assert_eq!(
        authority(&default),
        "objects.example.net",
        "a client does not send the port when it is the scheme's own, and a signature over a \
             host the request does not carry is refused"
    );
}

#[test]
fn a_key_with_awkward_characters_is_encoded_the_way_the_signature_expects() {
    assert_eq!(encode_path("/a b/c+d"), "/a%20b/c%2Bd");
    assert_eq!(
        encode_path("/SE_01/PA_02-0.ogg"),
        "/SE_01/PA_02-0.ogg",
        "an ordinary key must come through untouched, or every upload is signed for a path \
             the request does not ask for"
    );
}

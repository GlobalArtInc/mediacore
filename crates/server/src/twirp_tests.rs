use super::*;

fn headers(content_type: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers
}

#[test]
fn every_code_keeps_the_status_the_twirp_specification_gives_it() {
    let expected = [
        (Code::Canceled, 408),
        (Code::Unknown, 500),
        (Code::InvalidArgument, 400),
        (Code::Malformed, 400),
        (Code::DeadlineExceeded, 408),
        (Code::NotFound, 404),
        (Code::BadRoute, 404),
        (Code::AlreadyExists, 409),
        (Code::PermissionDenied, 403),
        (Code::Unauthenticated, 401),
        (Code::ResourceExhausted, 429),
        (Code::FailedPrecondition, 412),
        (Code::Aborted, 409),
        (Code::OutOfRange, 400),
        (Code::Unimplemented, 501),
        (Code::Internal, 500),
        (Code::Unavailable, 503),
        (Code::DataLoss, 500),
    ];

    for (code, status) in expected {
        assert_eq!(
            code.status().as_u16(),
            status,
            "`{}` answered with the wrong status: a caller's SDK reads the status before it \
                 reads the body, so this decides whether the call looks retryable",
            code.as_str()
        );
    }
}

#[test]
fn a_content_type_with_a_charset_is_still_json() {
    assert_eq!(
        Codec::from_headers(&headers("application/json; charset=utf-8")).unwrap(),
        Codec::Json
    );
    assert_eq!(
        Codec::from_headers(&headers("Application/JSON")).unwrap(),
        Codec::Json
    );
    assert_eq!(
        Codec::from_headers(&headers("application/protobuf")).unwrap(),
        Codec::Protobuf
    );
}

#[test]
fn a_body_we_cannot_read_is_a_routing_failure_not_a_bad_request() {
    let error = Codec::from_headers(&headers("text/plain")).unwrap_err();

    assert_eq!(error.code, Code::BadRoute);
    assert_eq!(
        error.code.status(),
        StatusCode::NOT_FOUND,
        "Twirp identifies a call by its content type as well as its path, so an unusable one \
             is a call that does not exist"
    );
    assert!(
        Codec::from_headers(&HeaderMap::new()).is_err(),
        "a request with no content type at all is not a JSON request by default"
    );
}

#[test]
fn a_request_without_a_token_is_unauthenticated() {
    let mut with_basic = HeaderMap::new();
    with_basic.insert(header::AUTHORIZATION, "Basic aGk6dGhlcmU=".parse().unwrap());

    assert_eq!(
        bearer(&HeaderMap::new()).unwrap_err().code,
        Code::Unauthenticated
    );
    assert_eq!(bearer(&with_basic).unwrap_err().code, Code::Unauthenticated);

    let mut with_bearer = HeaderMap::new();
    with_bearer.insert(header::AUTHORIZATION, "Bearer a.b.c".parse().unwrap());
    assert_eq!(bearer(&with_bearer).unwrap(), "a.b.c");
}

#[test]
fn the_scheme_is_read_the_way_rfc_7235_spells_it() {
    for spelling in [
        "Bearer a.b.c",
        "bearer a.b.c",
        "BEARER a.b.c",
        "BeArEr a.b.c",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, spelling.parse().unwrap());

        assert_eq!(
            bearer(&headers).unwrap(),
            "a.b.c",
            "`{spelling}` is a bearer token: the scheme is case-insensitive, and a caller \
                 who spells it differently is refused with nothing in the message to explain it"
        );
    }
}

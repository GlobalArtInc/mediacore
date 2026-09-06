use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use prost::Message;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A Twirp error code.
///
/// Twirp is the wire LiveKit's server API speaks, and its errors are the part
/// of it a caller reacts to: every SDK turns the `code` string back into a
/// typed error, and every HTTP proxy and retry policy in between reacts to the
/// status the code maps to. The mapping is fixed by the Twirp specification
/// rather than chosen by us -- an `unavailable` answered with 500 instead of
/// 503 turns a call worth retrying into a failure that looks permanent, and an
/// `internal` answered with 503 has a client hammering a server that will
/// never agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Canceled,
    Unknown,
    InvalidArgument,
    Malformed,
    DeadlineExceeded,
    NotFound,
    BadRoute,
    AlreadyExists,
    PermissionDenied,
    Unauthenticated,
    ResourceExhausted,
    FailedPrecondition,
    Aborted,
    OutOfRange,
    Unimplemented,
    Internal,
    Unavailable,
    DataLoss,
}

impl Code {
    /// Every code, in the order the Twirp specification lists them.
    ///
    /// The page at `/swagger` renders the code-to-status table off this, so
    /// what a caller is told about a status is what `status` below actually
    /// answers rather than a copy of it that was right once.
    pub const ALL: &'static [Self] = &[
        Self::Canceled,
        Self::Unknown,
        Self::InvalidArgument,
        Self::Malformed,
        Self::DeadlineExceeded,
        Self::NotFound,
        Self::BadRoute,
        Self::AlreadyExists,
        Self::PermissionDenied,
        Self::Unauthenticated,
        Self::ResourceExhausted,
        Self::FailedPrecondition,
        Self::Aborted,
        Self::OutOfRange,
        Self::Unimplemented,
        Self::Internal,
        Self::Unavailable,
        Self::DataLoss,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Canceled => "canceled",
            Self::Unknown => "unknown",
            Self::InvalidArgument => "invalid_argument",
            Self::Malformed => "malformed",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::NotFound => "not_found",
            Self::BadRoute => "bad_route",
            Self::AlreadyExists => "already_exists",
            Self::PermissionDenied => "permission_denied",
            Self::Unauthenticated => "unauthenticated",
            Self::ResourceExhausted => "resource_exhausted",
            Self::FailedPrecondition => "failed_precondition",
            Self::Aborted => "aborted",
            Self::OutOfRange => "out_of_range",
            Self::Unimplemented => "unimplemented",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
            Self::DataLoss => "data_loss",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            Self::Canceled | Self::DeadlineExceeded => StatusCode::REQUEST_TIMEOUT,
            Self::InvalidArgument | Self::Malformed | Self::OutOfRange => StatusCode::BAD_REQUEST,
            Self::NotFound | Self::BadRoute => StatusCode::NOT_FOUND,
            Self::AlreadyExists | Self::Aborted => StatusCode::CONFLICT,
            Self::PermissionDenied => StatusCode::FORBIDDEN,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
            Self::FailedPrecondition => StatusCode::PRECONDITION_FAILED,
            Self::Unimplemented => StatusCode::NOT_IMPLEMENTED,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Unknown | Self::Internal | Self::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A failed call, on its way back to the caller as a Twirp error body.
#[derive(Debug, Clone)]
pub struct Error {
    pub code: Code,
    pub message: String,
}

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn bad_route(message: impl Into<String>) -> Self {
        Self::new(Code::BadRoute, message)
    }

    pub fn malformed(message: impl Into<String>) -> Self {
        Self::new(Code::Malformed, message)
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(Code::InvalidArgument, message)
    }

    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(Code::Unauthenticated, message)
    }

    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(Code::PermissionDenied, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Code::NotFound, message)
    }

    pub fn unimplemented(message: impl Into<String>) -> Self {
        Self::new(Code::Unimplemented, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Code::Internal, message)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "code": self.code.as_str(),
            "msg": self.message,
        });

        (
            self.code.status(),
            [(header::CONTENT_TYPE, Codec::Json.content_type())],
            body.to_string(),
        )
            .into_response()
    }
}

/// How the bodies of one call are encoded.
///
/// Twirp offers exactly two, and the answer goes back in whichever the caller
/// used. Errors are the exception: they are always JSON, whatever the request
/// was, because a caller that could not be understood cannot be assumed to
/// understand a protobuf answer either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Json,
    Protobuf,
}

impl Codec {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Protobuf => "application/protobuf",
        }
    }

    /// Picks the codec a request asked for.
    ///
    /// Twirp calls an unusable content type a routing failure rather than a
    /// bad body, because the content type is part of what identifies the call.
    /// We are more forgiving than Twirp's own Go implementation, which compares
    /// the whole header against two exact strings: a `charset` parameter is
    /// accepted here, and so are the two other common spellings of protobuf.
    /// Neither can break a caller -- every LiveKit SDK sends one of the two
    /// exact strings anyway -- and both save somebody a morning.
    pub fn from_headers(headers: &HeaderMap) -> Result<Self, Error> {
        let value = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let media_type = value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();

        match media_type.as_str() {
            "application/json" => Ok(Self::Json),
            "application/protobuf" | "application/x-protobuf" | "application/proto" => {
                Ok(Self::Protobuf)
            }
            _ => Err(Error::bad_route(format!(
                "unexpected Content-Type `{value}`, Twirp speaks application/json or \
                 application/protobuf"
            ))),
        }
    }
}

pub fn decode<T>(codec: Codec, body: &Bytes) -> Result<T, Error>
where
    T: Message + Default + DeserializeOwned,
{
    match codec {
        Codec::Json => serde_json::from_slice(body).map_err(|error| {
            Error::malformed(format!("the json request could not be decoded: {error}"))
        }),
        Codec::Protobuf => T::decode(body.as_ref()).map_err(|error| {
            Error::malformed(format!(
                "the protobuf request could not be decoded: {error}"
            ))
        }),
    }
}

pub fn encode<T>(codec: Codec, message: &T) -> Result<Response, Error>
where
    T: Message + Serialize,
{
    let body = match codec {
        Codec::Json => serde_json::to_vec(message).map_err(|error| {
            Error::internal(format!("the answer could not be encoded: {error}"))
        })?,
        Codec::Protobuf => message.encode_to_vec(),
    };

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, codec.content_type())],
        body,
    )
        .into_response())
}

/// Pulls the bearer token out of an `Authorization` header.
///
/// Nothing here decides whether the token is any good -- that is the service's
/// job, and it needs the claims to say which grant a method wants. This only
/// insists the caller brought one at all, so a request with no credentials
/// reads as `unauthenticated` rather than as a permission the token failed to
/// carry.
///
/// The scheme is compared case-insensitively because RFC 7235 says it is one.
/// No SDK sends anything but `Bearer`, so this is for whoever is holding a
/// terminal at two in the morning.
pub fn bearer(headers: &HeaderMap) -> Result<&str, Error> {
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| Error::unauthenticated("no Authorization header on the request"))?
        .to_str()
        .map_err(|_| Error::unauthenticated("the Authorization header is not readable text"))?;

    let (scheme, token) = value
        .split_once(' ')
        .ok_or_else(|| Error::unauthenticated("the Authorization header is not a bearer token"))?;

    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(Error::unauthenticated(format!(
            "the Authorization header carries `{scheme}`, and this is a bearer token API"
        )));
    }

    let token = token.trim();
    if token.is_empty() {
        return Err(Error::unauthenticated("the bearer token is empty"));
    }

    Ok(token)
}

#[cfg(test)]
mod tests {
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
}

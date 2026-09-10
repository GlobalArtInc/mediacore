use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mediacore_auth::{Claims, VideoGrant};
use mediacore_config::Config;
use mediacore_server::rooms::{self, Grant, Method, Support};
use mediacore_server::{http, state::AppState, swagger};
use prost_reflect::{DescriptorPool, EnumDescriptor, FieldDescriptor, Kind};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const API_KEY: &str = "devkey";
const API_SECRET: &str = "devsecretdevsecretdevsecretdevsecret";

/// The room every call in here names, so that a `roomAdmin` token can be
/// scoped to it.
const ROOM: &str = "standup";

async fn spawn_server() -> SocketAddr {
    let mut config = Config::parse(&format!("keys:\n  {API_KEY}: {API_SECRET}\n")).unwrap();
    config.port = 0;
    config.rtc.udp_port = 0;
    config.rtc.port_range_start = 0;
    config.rtc.port_range_end = 0;
    config.rtc.bind_address = "127.0.0.1".to_owned();
    // A directory of this server's own for session records: a test must
    // never write into the tree, and two servers sharing a store would
    // read each other's meetings back.
    config.sessions.directory = tempfile::TempDir::new().unwrap().keep();

    let state = AppState::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::router(state);

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    addr
}

fn token(video: VideoGrant) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    mediacore_auth::issue(
        API_SECRET,
        &Claims {
            exp,
            iss: API_KEY.to_owned(),
            video,
            ..Claims::default()
        },
    )
    .unwrap()
}

/// A token carrying exactly one grant, scoped to `room` where the grant is a
/// scoped one.
fn token_for(grant: Grant, room: &str) -> String {
    let mut video = VideoGrant::default();
    match grant {
        Grant::Create => video.room_create = Some(true),
        Grant::List => video.room_list = Some(true),
        Grant::Admin => {
            video.room_admin = Some(true);
            video.room = room.to_owned();
        }
    }

    token(video)
}

struct Answer {
    status: u16,
    content_type: String,
    headers: String,
    body: Vec<u8>,
}

impl Answer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("the answer is not json ({error}): {}", self.text()))
    }

    fn code(&self) -> String {
        self.json()
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn message(&self) -> String {
        self.json()
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn header(&self, name: &str) -> String {
        self.headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_default()
    }
}

async fn request(
    addr: SocketAddr,
    verb: &str,
    path: &str,
    content_type: Option<&str>,
    authorization: Option<&str>,
    body: &[u8],
) -> Answer {
    let exchange = async {
        let mut socket = TcpStream::connect(addr).await.expect("connect");
        let mut head = format!(
            "{verb} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(content_type) = content_type {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        if let Some(authorization) = authorization {
            head.push_str(&format!("Authorization: Bearer {authorization}\r\n"));
        }
        head.push_str("\r\n");

        socket.write_all(head.as_bytes()).await.expect("request");
        socket.write_all(body).await.expect("request body");

        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.expect("response");
        raw
    };

    let raw = tokio::time::timeout(Duration::from_secs(10), exchange)
        .await
        .expect("the server did not answer in time");

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("http response without a body");
    let headers = String::from_utf8_lossy(&raw[..split]).into_owned();

    Answer {
        status: headers
            .split_whitespace()
            .nth(1)
            .expect("http status line")
            .parse()
            .expect("numeric http status"),
        content_type: headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-type")
                    .then(|| value.trim().to_ascii_lowercase())
            })
            .unwrap_or_default(),
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

async fn get(addr: SocketAddr, path: &str) -> Answer {
    request(addr, "GET", path, None, None, b"").await
}

async fn post(addr: SocketAddr, method: &str, authorization: &str, body: &Value) -> Answer {
    request(
        addr,
        "POST",
        &format!("{}{method}", rooms::PATH_PREFIX),
        Some("application/json"),
        Some(authorization),
        body.to_string().as_bytes(),
    )
    .await
}

async fn specification(addr: SocketAddr) -> Value {
    get(addr, swagger::SPEC_PATH).await.json()
}

fn operation<'a>(document: &'a Value, method: &str) -> &'a Value {
    let path = format!("{}{method}", rooms::PATH_PREFIX);

    document["paths"]
        .get(&path)
        .unwrap_or_else(|| panic!("`{path}` is not on the page at all"))
        .get("post")
        .unwrap_or_else(|| panic!("`{path}` is documented as something other than a POST"))
}

fn pool() -> DescriptorPool {
    DescriptorPool::decode(mediacore_proto::FILE_DESCRIPTOR_SET)
        .expect("the descriptor set the build keeps")
}

/// A request body carrying exactly the fields the table says the method
/// insists on, so the call gets as far as the grant check.
fn required_body(method: &Method) -> Value {
    let mut body = serde_json::Map::new();

    for field in method.requires {
        let value = match *field {
            "name" | "room" => ROOM,
            "identity" => "somebody",
            "trackSid" => "TR_nothing",
            other => panic!(
                "rooms::METHODS says `{}` requires `{other}`, and this test does not know what a \
                 plausible value for it looks like -- teach it here rather than dropping the field",
                method.name
            ),
        };
        body.insert((*field).to_owned(), json!(value));
    }

    Value::Object(body)
}

#[tokio::test]
async fn the_page_is_served_and_says_that_try_it_out_is_real() {
    let addr = spawn_server().await;
    let answer = get(addr, swagger::PAGE_PATH).await;

    assert_eq!(answer.status, 200);
    assert!(
        answer.content_type.starts_with("text/html"),
        "the page came back as `{}`, and a browser will not render it",
        answer.content_type
    );

    let page = answer.text();
    assert!(
        page.contains("not a sandbox") && page.contains("DeleteRoom"),
        "the page does not warn that Try it out is a live call: somebody reads `DeleteRoom` as a \
         description and presses the button on a meeting that is happening"
    );
    assert!(
        answer
            .header("content-security-policy")
            .contains("script-src 'self'"),
        "the page is served without a policy confining script to this origin: {}",
        answer.header("content-security-policy")
    );
    assert!(
        !page.contains("<script>"),
        "the page carries an inline script, which the content security policy above forbids -- it \
         would load with a blank body and nothing in the page to say why"
    );
}

#[tokio::test]
async fn the_specification_is_served_and_parses() {
    let addr = spawn_server().await;
    let answer = get(addr, swagger::SPEC_PATH).await;

    assert_eq!(answer.status, 200);
    assert!(
        answer.content_type.starts_with("application/json"),
        "the specification came back as `{}`",
        answer.content_type
    );

    let document = answer.json();
    assert_eq!(
        document["openapi"].as_str(),
        Some("3.0.3"),
        "the version tells every generator which document this is; without one, a caller's client \
         generator refuses the file"
    );
    assert!(
        document["info"]["title"]
            .as_str()
            .unwrap_or_default()
            .contains("RoomService"),
        "the document does not say what service it describes"
    );
    assert!(
        !document["paths"].as_object().unwrap().is_empty(),
        "a specification with no paths in it describes nothing"
    );

    let description = document["info"]["description"].as_str().unwrap_or_default();
    assert!(
        description.contains(rooms::PATH_PREFIX),
        "the page never says where to post, so a reader has to infer the Twirp path from the \
         operation ids"
    );
    assert!(
        description.contains(rooms::ALIAS_PREFIX),
        "the page does not mention that `{}` answers the same calls. Somebody integrating with a \
         LiveKit server SDK reads this page, sees only the canonical path, and concludes their \
         SDK cannot be used against this server -- which is the one thing the alias exists to \
         make untrue.",
        rooms::ALIAS_SERVICE
    );
}

#[tokio::test]
async fn every_method_the_server_answers_is_on_the_page() {
    let addr = spawn_server().await;
    let document = specification(addr).await;

    for method in rooms::METHODS {
        let Support::Served(_) = method.support else {
            continue;
        };
        let operation = operation(&document, method.name);

        assert_eq!(
            operation["x-mediacore-implemented"],
            json!(true),
            "`{}` is answered by this server and the page says it is not",
            method.name
        );
        assert!(
            operation["requestBody"]["content"]["application/json"]["schema"]["$ref"].is_string(),
            "`{}` is documented with no request body, so nobody can tell what to send it",
            method.name
        );
        assert!(
            operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"]
                .is_string(),
            "`{}` is documented with no successful answer, so nobody can tell what comes back",
            method.name
        );
    }
}

#[tokio::test]
async fn the_methods_we_refuse_are_marked_as_refused() {
    let addr = spawn_server().await;
    let document = specification(addr).await;

    for method in rooms::METHODS {
        let Support::Refused(reason) = method.support else {
            continue;
        };
        let operation = operation(&document, method.name);

        assert_eq!(
            operation["x-mediacore-implemented"],
            json!(false),
            "`{}` answers 501 and the page offers it as though it worked",
            method.name
        );
        assert!(
            operation["responses"]["200"].is_null(),
            "`{}` is documented with a successful answer it never gives",
            method.name
        );
        assert!(
            operation["responses"]["501"].is_object(),
            "`{}` is refused and the page does not say so under any status",
            method.name
        );
        assert!(
            operation["description"]
                .as_str()
                .unwrap_or_default()
                .contains(reason),
            "`{}` does not carry the reason it is refused, so a reader is left with a bare 501",
            method.name
        );

        let answered = post(
            addr,
            method.name,
            &token_for(Grant::Admin, ROOM),
            &json!({}),
        )
        .await;
        assert_eq!(
            answered.status, 501,
            "the page says `{}` is not implemented and the server answered {}",
            method.name, answered.status
        );
        assert_eq!(answered.code(), "unimplemented");
    }
}

#[tokio::test]
async fn the_security_requirement_is_on_the_operation_and_not_only_in_the_prose() {
    let addr = spawn_server().await;
    let document = specification(addr).await;
    let schemes = document["components"]["securitySchemes"]
        .as_object()
        .expect("the document declares no security schemes at all");

    for (name, scheme) in schemes {
        assert_eq!(
            scheme["type"],
            json!("http"),
            "`{name}` is not an http scheme, so the authorize box will not send a header for it"
        );
        assert_eq!(
            scheme["scheme"],
            json!("bearer"),
            "`{name}` does not send `Authorization: Bearer <token>`, which is the only credential \
             this API reads"
        );
    }

    for method in rooms::METHODS {
        let operation = operation(&document, method.name);
        let requirements = operation["security"]
            .as_array()
            .unwrap_or_else(|| panic!("`{}` carries no security requirement", method.name));

        assert!(
            !requirements.is_empty(),
            "`{}` is documented as needing no credential, and every method here needs one",
            method.name
        );

        for requirement in requirements {
            for named in requirement
                .as_object()
                .expect("a security requirement is an object")
                .keys()
            {
                assert!(
                    schemes.contains_key(named),
                    "`{}` asks for a scheme `{named}` the document never declares, so the \
                     authorize dialog has no box to fill in for it",
                    method.name
                );
            }
        }

        if let Support::Served(grant) = method.support {
            assert_eq!(
                requirements,
                &vec![json!({ grant.claim(): [] })],
                "`{}` needs `{}` and the document says something else -- this is the field a \
                 generated client enforces, and prose beside it does not travel",
                method.name,
                grant.claim()
            );
            assert_eq!(
                operation["x-mediacore-grant"],
                json!(grant.claim()),
                "`{}` does not name its grant where a reader skimming the page will see it",
                method.name
            );
        }
    }
}

#[tokio::test]
async fn the_grant_the_page_names_is_the_grant_the_server_asks_for() {
    let addr = spawn_server().await;

    for method in rooms::METHODS {
        let Support::Served(wanted) = method.support else {
            continue;
        };
        let body = required_body(method);

        for grant in [Grant::Create, Grant::List, Grant::Admin] {
            let answered = post(addr, method.name, &token_for(grant, ROOM), &body).await;

            if grant == wanted {
                assert_ne!(
                    answered.code(),
                    "permission_denied",
                    "the page tells a caller `{}` needs `{}`, and a token carrying exactly that \
                     was refused: {}",
                    method.name,
                    wanted.claim(),
                    answered.message()
                );
                assert_ne!(
                    answered.code(),
                    "unauthenticated",
                    "`{}` refused a token this server signed",
                    method.name
                );
            } else {
                assert_eq!(
                    answered.code(),
                    "permission_denied",
                    "`{}` is documented as needing `{}` and a `{}` token got through: a reader \
                     mints the narrow token the page asks for and hands out a wider one than they \
                     meant",
                    method.name,
                    wanted.claim(),
                    grant.claim()
                );
            }
        }

        if wanted.scoped() {
            let elsewhere = post(addr, method.name, &token_for(wanted, "payroll"), &body).await;

            assert_eq!(
                elsewhere.code(),
                "permission_denied",
                "the page says `{}` is scoped by the token's `room` claim, and a token for \
                 `payroll` moderated `{ROOM}` -- which makes every admin token a master key",
                method.name
            );
        }
    }
}

/// The session endpoints are not Twirp and not generated from a descriptor,
/// so nothing else would notice if the page and the router drifted apart. This
/// is the half that watches the page; `tests/sessions.rs` calls every path in
/// the same table and watches the server.
#[tokio::test]
async fn every_session_endpoint_is_on_the_page_with_a_grant_on_it() {
    let addr = spawn_server().await;
    let document = specification(addr).await;

    for endpoint in mediacore_server::sessions::ENDPOINTS {
        let operation = document["paths"][endpoint.path]["get"].clone();

        assert!(
            operation.is_object(),
            "`GET {}` is served by this server and missing from the page, so the only place a \
             caller could learn it exists is the source",
            endpoint.path
        );
        assert_eq!(operation["operationId"], json!(endpoint.operation));
        assert_eq!(
            operation["security"],
            json!([{ Grant::List.claim(): [] }, { Grant::Admin.claim(): [] }]),
            "`{}` is documented without the grants it actually wants; a generated client sends \
             no token and gets a 401 it cannot explain",
            endpoint.path
        );
        assert!(
            operation["responses"]["200"]["content"]["application/json"]["schema"].is_object(),
            "`{}` is documented with no successful answer, so nobody can tell what comes back",
            endpoint.path
        );

        let parameters = operation["parameters"]
            .as_array()
            .unwrap_or_else(|| panic!("`{}` documents no parameter", endpoint.path));
        assert_eq!(parameters[0]["name"], json!(endpoint.parameter.0));
        // Documented where it is actually written: a query parameter listed as
        // a path one is a page telling a caller to build an address that does
        // not exist.
        assert_eq!(
            parameters[0]["in"],
            json!(endpoint.located.as_str()),
            "`{}` documents its `{}` parameter in the wrong place",
            endpoint.path,
            endpoint.parameter.0
        );
    }

    assert!(
        document["components"]["schemas"]["session.Session"]["properties"]["participants"]
            .is_object(),
        "the page describes a meeting without saying who was at it"
    );
}

/// The two session schemas are the only hand-written ones on the page -- there
/// is no protobuf behind a meeting record -- so they are the two that can drift
/// away from what the server actually answers with. This is the check that
/// makes them drift loudly: the record is serialised exactly as the endpoints
/// serialise it, and every key it produces has to be a key the page describes.
#[tokio::test]
async fn the_meeting_schemas_name_every_field_a_meeting_answers_with() {
    let addr = spawn_server().await;
    let document = specification(addr).await;

    let session = mediacore_session::Session {
        id: "SE_0123456789ab".to_owned(),
        room: ROOM.to_owned(),
        started_at_ms: 1_757_155_200_000,
        ended_at_ms: Some(1_757_156_740_000),
        ended_by: Some(mediacore_session::Ending::Grace),
        participants: vec![mediacore_session::Attendance {
            sid: "PA_0123456789ab".to_owned(),
            identity: "alice".to_owned(),
            name: "Alice".to_owned(),
            kind: mediacore_session::Kind::Standard,
            attributes: std::collections::HashMap::from([(
                "accountId".to_owned(),
                "AC_9271".to_owned(),
            )]),
            joined_at_ms: 1_757_155_188_000,
            left_at_ms: Some(1_757_156_650_000),
        }],
        metadata: Some(r#"{"tenant":"acme"}"#.to_owned()),
    };
    let answered = serde_json::to_value(&session).expect("a meeting record encodes");

    // The recordings endpoint answers with the recording's own fields and the
    // work behind it on the same object, so the shape held against the page is
    // built the same way here.
    let mut recording = serde_json::to_value(mediacore_session::Recording {
        id: "RE_0123456789ab".to_owned(),
        session: "SE_0123456789ab".to_owned(),
        participant: "PA_0123456789ab".to_owned(),
        segment: 0,
        local_path: std::path::PathBuf::from("/var/lib/mediacore/audio/x.ogg"),
        node: Some("ND_one".to_owned()),
        object_key: Some("SE_0123456789ab/PA_0123456789ab-0.ogg".to_owned()),
        bytes: Some(1_048_576),
        sample_rate: 48_000,
        channels: 1,
        started_at_ms: 1_757_155_200_000,
        ended_at_ms: Some(1_757_156_740_000),
        offset_ms: 0,
        duration_ms: Some(1_540_000),
    })
    .expect("a recording encodes");
    let work = serde_json::to_value(mediacore_session::Work {
        recording: "RE_0123456789ab".to_owned(),
        stage: mediacore_session::Stage::Upload,
        state: mediacore_session::WorkState::Done,
        attempts: 1,
        due_at_ms: 1_757_156_740_000,
        claimed_by: None,
        claim_ends_at_ms: None,
        done_at_ms: Some(1_757_156_742_000),
        last_error: None,
    })
    .expect("a piece of work encodes");
    recording["work"] = serde_json::Value::Array(vec![work.clone()]);

    for (schema, body) in [
        ("session.Session", &answered),
        ("session.Attendance", &answered["participants"][0]),
        ("session.Recording", &recording),
        ("session.RecordingWork", &work),
    ] {
        let produced: BTreeSet<String> = body
            .as_object()
            .expect("the record encodes as an object")
            .keys()
            .cloned()
            .collect();
        let documented: BTreeSet<String> = document["components"]["schemas"][schema]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("`{schema}` is documented with no properties"))
            .keys()
            .cloned()
            .collect();

        assert_eq!(
            produced, documented,
            "`{schema}` on the page is not what this server answers with. A field added to the \
             record and not to the page is a field an integrator never finds out about; one on \
             the page and not in the record is one they write code against and never receive."
        );
    }
}

#[tokio::test]
async fn the_table_and_the_service_name_the_same_methods() {
    let pool = pool();
    let service = pool
        .get_service_by_name(rooms::SERVICE)
        .unwrap_or_else(|| panic!("{} is in the descriptor set", rooms::SERVICE));

    let declared = service
        .methods()
        .map(|method| method.name().to_owned())
        .collect::<BTreeSet<String>>();
    let described = rooms::METHODS
        .iter()
        .map(|method| method.name.to_owned())
        .collect::<BTreeSet<String>>();

    assert_eq!(
        declared, described,
        "the `.proto` and `rooms::METHODS` disagree about what this service has. A method in the \
         proto and not the table is invisible on the page and answers `bad_route`; a method in the \
         table and not the proto is a path nobody can call. Classify it as served or refused."
    );
}

#[tokio::test]
async fn every_schema_on_the_page_is_the_descriptor_field_for_field() {
    let addr = spawn_server().await;
    let document = specification(addr).await;
    let pool = pool();
    let schemas = document["components"]["schemas"]
        .as_object()
        .expect("the document declares no schemas");

    let mut checked = 0;
    for (name, schema) in schemas {
        if let Some(message) = pool.get_message_by_name(name) {
            let documented = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("`{name}` is documented with no properties"))
                .keys()
                .cloned()
                .collect::<BTreeSet<String>>();
            let real = message
                .fields()
                .map(|field| field.json_name().to_owned())
                .collect::<BTreeSet<String>>();

            assert_eq!(
                documented, real,
                "`{name}` on the page is not the `{name}` on the wire. This document is generated \
                 from the descriptors, so the only way the two differ is that somebody edited the \
                 document by hand -- and a hand-edited copy is a second source of truth that will \
                 be wrong within a month."
            );
            checked += 1;
        } else if let Some(enumeration) = pool.get_enum_by_name(name) {
            let documented = schema["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("`{name}` is documented as an enum with no members"))
                .iter()
                .map(|value| value.as_str().unwrap_or_default().to_owned())
                .collect::<BTreeSet<String>>();
            let real = enumeration
                .values()
                .map(|value| value.name().to_owned())
                .collect::<BTreeSet<String>>();

            assert_eq!(
                documented, real,
                "the members of `{name}` on the page are not the members in the `.proto`"
            );
            checked += 1;
        }
    }

    assert!(
        checked > 20,
        "only {checked} schemas traced back to the descriptor pool, which means the document \
         stopped being generated from it and this test stopped guarding anything"
    );
}

#[tokio::test]
async fn a_sixty_four_bit_field_is_documented_as_the_string_it_travels_as() {
    let addr = spawn_server().await;
    let document = specification(addr).await;
    let room = &document["components"]["schemas"]["mediacore.Room"]["properties"];

    assert_eq!(
        room["creationTime"]["type"],
        json!("string"),
        "`creationTime` is an int64 and protobuf JSON sends it as a string. Documented as a number, \
         a generated client parses it into a double and quietly loses the low bits."
    );
    assert_eq!(
        room["maxParticipants"]["type"],
        json!("integer"),
        "a uint32 fits in a JSON number and is sent as one; calling it a string sends a caller \
         looking for quotes that are not there"
    );
}

#[tokio::test]
async fn the_error_body_on_the_page_is_the_error_body_on_the_wire() {
    let addr = spawn_server().await;
    let document = specification(addr).await;
    let error = &document["components"]["schemas"]["twirp.Error"];

    let refused = post(
        addr,
        "CreateRoom",
        &token_for(Grant::List, ROOM),
        &json!({"name": ROOM}),
    )
    .await;
    assert_eq!(refused.status, 403);

    let body = refused.json();
    let documented = error["properties"]
        .as_object()
        .expect("the error schema has no properties")
        .keys()
        .cloned()
        .collect::<BTreeSet<String>>();
    let sent = body
        .as_object()
        .expect("a twirp error is an object")
        .keys()
        .cloned()
        .collect::<BTreeSet<String>>();

    assert_eq!(
        sent, documented,
        "the failure body the server sends is not the one the page describes, and an SDK builds \
         its typed errors out of exactly these fields"
    );

    let codes = error["properties"]["code"]["enum"]
        .as_array()
        .expect("the error schema does not list the codes")
        .iter()
        .map(|code| code.as_str().unwrap_or_default().to_owned())
        .collect::<Vec<String>>();
    assert!(
        codes.contains(&refused.code()),
        "the server answered `{}` and the page does not list it among the codes this service \
         gives: {codes:?}",
        refused.code()
    );
}

#[tokio::test]
async fn the_create_room_tables_account_for_every_field_of_the_request() {
    let pool = pool();
    let request = pool
        .get_message_by_name("mediacore.CreateRoomRequest")
        .expect("mediacore.CreateRoomRequest is in the descriptor set");

    let mut described = Vec::new();
    for (field, _) in rooms::CREATE_HONOURED
        .iter()
        .chain(rooms::CREATE_IGNORED)
        .chain(rooms::CREATE_REFUSED)
    {
        described.push((*field).to_owned());
    }

    let unique = described.iter().cloned().collect::<BTreeSet<String>>();
    assert_eq!(
        unique.len(),
        described.len(),
        "a field is in two of the CREATE_* lists at once, so the page says two things about it"
    );

    let real = request
        .fields()
        .map(|field| field.json_name().to_owned())
        .collect::<BTreeSet<String>>();

    assert_eq!(
        unique, real,
        "CREATE_HONOURED, CREATE_IGNORED and CREATE_REFUSED do not account for every field of \
         CreateRoomRequest. A field nobody classified is one the page says nothing about and the \
         server silently drops -- which is how a caller comes to believe a meeting is being \
         recorded."
    );
}

#[tokio::test]
async fn every_field_the_page_says_we_refuse_is_really_refused() {
    let addr = spawn_server().await;
    let pool = pool();
    let request = pool
        .get_message_by_name("mediacore.CreateRoomRequest")
        .expect("mediacore.CreateRoomRequest is in the descriptor set");

    for (field, _) in rooms::CREATE_REFUSED {
        let descriptor = request
            .get_field_by_json_name(field)
            .unwrap_or_else(|| panic!("CREATE_REFUSED names `{field}`, which the request has not"));

        let mut body = serde_json::Map::new();
        body.insert("name".to_owned(), json!("refusal-check"));
        body.insert((*field).to_owned(), sample(&descriptor));

        let answered = post(
            addr,
            "CreateRoom",
            &token_for(Grant::Create, ROOM),
            &Value::Object(body),
        )
        .await;

        assert_eq!(
            answered.code(),
            "unimplemented",
            "the page says `{field}` is refused and the server took it: a caller reads the page, \
             sends the field, gets a 200, and believes something is happening that is not"
        );
        assert!(
            answered.message().contains(field),
            "`{field}` was refused without being named, so a caller cannot tell which half of \
             their request we could not do: {}",
            answered.message()
        );
    }
}

/// A value of the right shape for one field, read off the descriptor rather
/// than written down, so this test keeps working when a refused field changes
/// type.
fn sample(field: &FieldDescriptor) -> Value {
    if field.is_map() {
        let value = field
            .kind()
            .as_message()
            .expect("a map field's kind is its entry message")
            .map_entry_value_field();

        return json!({ "any": scalar(&value) });
    }

    if field.is_list() {
        return json!([scalar(field)]);
    }

    scalar(field)
}

fn scalar(field: &FieldDescriptor) -> Value {
    match field.kind() {
        Kind::Double | Kind::Float => json!(1.0),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 | Kind::Uint32 | Kind::Fixed32 => json!(1),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 | Kind::Uint64 | Kind::Fixed64 => json!("1"),
        Kind::Bool => json!(true),
        Kind::String => json!("something"),
        Kind::Bytes => json!("aGk="),
        Kind::Message(_) => json!({}),
        Kind::Enum(enumeration) => json!(first_member(&enumeration)),
    }
}

fn first_member(enumeration: &EnumDescriptor) -> String {
    enumeration
        .values()
        .next()
        .expect("a protobuf enum has at least one member")
        .name()
        .to_owned()
}

#[tokio::test]
async fn the_page_loads_nothing_this_server_does_not_serve() {
    let addr = spawn_server().await;
    let page = get(addr, swagger::PAGE_PATH).await.text();

    let referenced = page
        .split('"')
        .filter(|part| part.starts_with("/swagger/"))
        .map(str::to_owned)
        .collect::<Vec<String>>();

    assert!(
        referenced.len() >= 4,
        "the page references {} of its own assets, which is fewer than it needs to render",
        referenced.len()
    );
    assert!(
        !page.contains("//cdn.") && !page.contains("https://unpkg") && !page.contains("//cdnjs."),
        "the page pulls something off a content delivery network: a deployment with no route to \
         the internet gets a blank page, and the content security policy would refuse it anyway"
    );

    for path in referenced {
        let answer = get(addr, &path).await;
        assert_eq!(
            answer.status, 200,
            "the page asks for `{path}` and this server answers {}",
            answer.status
        );
        assert!(
            !answer.body.is_empty(),
            "`{path}` came back empty, so the page renders without it and says nothing about why"
        );
    }
}

#[tokio::test]
async fn the_licence_notice_travels_with_the_thing_it_covers() {
    let addr = spawn_server().await;
    let notice = get(addr, swagger::NOTICE_PATH).await;

    assert_eq!(notice.status, 200);

    let text = notice.text();
    assert!(
        text.contains("swagger-ui") && text.contains("Apache License"),
        "the notice does not name swagger-ui and reproduce the licence it is under, which \
         Apache-2.0 asks for rather than suggests"
    );
    assert!(
        text.contains(swagger::UI_VERSION),
        "the notice does not name the version that is actually served: {}",
        swagger::UI_VERSION
    );

    let page = get(addr, swagger::PAGE_PATH).await.text();
    assert!(
        page.contains(swagger::NOTICE_PATH),
        "nothing on the page links to the notice, so it is served and never read"
    );
    assert!(
        page.contains(swagger::UI_VERSION),
        "the page loads a swagger-ui whose version is not {} -- the routes, the page and the \
         notice have to name one version between them, or an upgrade leaves two of the three \
         describing the old one",
        swagger::UI_VERSION
    );
}

#[tokio::test]
async fn no_copy_of_the_specification_is_checked_into_the_tree() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/server sits two levels below the repository root")
        .to_path_buf();

    let mut found = Vec::new();
    look_for_a_specification(&root, &mut found);

    assert!(
        found.is_empty(),
        "an OpenAPI document is checked in at {found:?}. This one is generated from the protobuf \
         descriptors on every build; a copy on disk is a second source of truth, and the copy is \
         the one that goes stale, because nothing fails when it does."
    );
}

fn look_for_a_specification(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if path.is_dir() {
            if name == "target" || name == ".git" || name == "node_modules" {
                continue;
            }
            look_for_a_specification(&path, found);
            continue;
        }

        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        if !matches!(extension, "json" | "yaml" | "yml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };

        let declares_openapi = text.lines().take(40).any(|line| {
            let line = line.trim();
            (line.starts_with("\"openapi\"") || line.starts_with("openapi:")) && line.contains("3.")
        });
        if declares_openapi {
            found.push(path);
        }
    }
}

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use prost_reflect::{
    DescriptorPool, EnumDescriptor, FieldDescriptor, Kind, MessageDescriptor, MethodDescriptor,
};
use serde_json::{Map, Value, json};

use crate::rooms::{self, Grant, Method, Support};
use crate::sessions::{self, Answer, Endpoint};
use crate::twirp::Code;
use mediacore_session::{DEFAULT_PAGE, MAX_PAGE};

/// The OpenAPI document for `mediacore.RoomService`, generated from the
/// protobuf descriptors the build keeps.
///
/// Nothing here is written twice. The paths, the request and response bodies,
/// the field names, the enum members and most of the prose come off
/// `mediacore_proto::FILE_DESCRIPTOR_SET` -- the same bytes the wire types are
/// generated from -- and the rest comes off `rooms::METHODS`, the table the
/// handler itself reads. Change a `.proto` and the page changes with it in the
/// same build; a document written by hand instead is correct for about a month
/// and then quietly lies to whoever is integrating against it.
///
/// Built once, on the first request for it, and kept: it is a few hundred
/// kilobytes of JSON assembled from a 190 kB descriptor set, which is cheap
/// once and silly on every refresh.
pub fn document() -> &'static Value {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();

    DOCUMENT.get_or_init(build)
}

/// The same document as the bytes we serve.
pub fn json() -> &'static str {
    static JSON: OnceLock<String> = OnceLock::new();

    JSON.get_or_init(|| {
        serde_json::to_string_pretty(document()).expect("a document we built is serialisable")
    })
}

/// OpenAPI 3.0.3, not 3.1.
///
/// The page is not the only reader. A caller runs a generator over this
/// document to get a typed client, and 3.0.3 is the version every generator
/// handles; 3.1 buys us nothing here, because protobuf JSON needs no `null`
/// types, no tuple arrays and no dialect of its own.
const OPENAPI_VERSION: &str = "3.0.3";

const SERVED_TAG: &str = "Room service";
const REFUSED_TAG: &str = "Not implemented here";
const SESSIONS_TAG: &str = "Sessions";

/// The record of one meeting, and one stretch of somebody being at it.
///
/// The only two schemas on this page not read off the descriptors, because
/// there is no protobuf behind them: LiveKit has no session record, so there
/// is nothing to vendor and nothing to generate from. They are held against
/// the real JSON by a test rather than by a descriptor.
const SESSION_SCHEMA: &str = "session.Session";
const ATTENDANCE_SCHEMA: &str = "session.Attendance";

/// Twirp's error body, the shape every one of these methods fails in.
const ERROR_SCHEMA: &str = "twirp.Error";

fn build() -> Value {
    let pool = DescriptorPool::decode(mediacore_proto::FILE_DESCRIPTOR_SET)
        .expect("the descriptor set the build writes is a descriptor set");
    let service = pool
        .get_service_by_name(rooms::SERVICE)
        .unwrap_or_else(|| panic!("{} is missing from the descriptor set", rooms::SERVICE));
    let comments = Comments::read(&pool);
    let mut schemas = Schemas::new(&comments);

    let mut paths = Map::new();
    for method in rooms::METHODS {
        let descriptor = service
            .methods()
            .find(|candidate| candidate.name() == method.name)
            .unwrap_or_else(|| {
                panic!(
                    "rooms::METHODS names `{}`, which {} does not have",
                    method.name,
                    rooms::SERVICE
                )
            });

        let mut operation = Map::new();
        operation.insert("operationId".to_owned(), json!(method.name));
        operation.insert(
            "summary".to_owned(),
            json!(summary(method, &comments, &descriptor)),
        );
        operation.insert(
            "description".to_owned(),
            json!(describe(method, &comments, &descriptor)),
        );
        operation.insert("tags".to_owned(), json!([tag(method)]));
        operation.insert("security".to_owned(), security(method));
        operation.insert("x-mediacore-implemented".to_owned(), json!(served(method)));
        if let Support::Served(grant) = method.support {
            operation.insert("x-mediacore-grant".to_owned(), json!(grant.claim()));
        }
        operation.insert(
            "requestBody".to_owned(),
            json!({
                "required": true,
                "content": bodies(&mut schemas, &descriptor.input()),
            }),
        );
        operation.insert(
            "responses".to_owned(),
            responses(method, &mut schemas, &descriptor.output()),
        );

        paths.insert(
            format!("{}{}", rooms::PATH_PREFIX, method.name),
            json!({ "post": Value::Object(operation) }),
        );
    }

    for endpoint in sessions::ENDPOINTS {
        paths.insert(endpoint.path.to_owned(), session_path(endpoint));
    }

    let mut components = schemas.finish();
    components.insert(ERROR_SCHEMA.to_owned(), error_schema());
    components.insert(SESSION_SCHEMA.to_owned(), session_schema());
    components.insert(ATTENDANCE_SCHEMA.to_owned(), attendance_schema());

    json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": rooms::SERVICE,
            "version": mediacore_proto::SERVER_VERSION,
            "description": preamble(),
            "license": {
                "name": "Apache-2.0",
                "url": "https://www.apache.org/licenses/LICENSE-2.0",
            },
        },
        "tags": [
            {
                "name": SERVED_TAG,
                "description": "Answered by this server.",
            },
            {
                "name": REFUSED_TAG,
                "description":
                    "On the wire, and refused with `unimplemented` (501). Listed so that a \
                     caller reads why rather than guessing from a bare status.",
            },
            {
                "name": SESSIONS_TAG,
                "description":
                    "What happened in a room, as opposed to what is happening in it. Plain \
                     `GET`s, not Twirp: LiveKit has no session record, so there is no generated \
                     client anywhere that would call these and nothing to be compatible with.",
            },
        ],
        "paths": Value::Object(paths),
        "components": {
            "securitySchemes": security_schemes(),
            "schemas": Value::Object(components),
        },
    })
}

fn served(method: &Method) -> bool {
    matches!(method.support, Support::Served(_))
}

fn tag(method: &Method) -> &'static str {
    if served(method) {
        SERVED_TAG
    } else {
        REFUSED_TAG
    }
}

/// What the authorize dialog offers: one row per grant, all of them bearer
/// tokens, each named the way the claim is named in the token.
///
/// One anonymous `bearer` scheme would have been one box to fill in, but then
/// the document says nothing about which grant a method wants and the page
/// would have to explain it in prose beside the operations -- which is exactly
/// the drift this whole page exists to avoid. The cost is that a token pasted
/// into the wrong row would send no header at all; `swagger-init.js` covers
/// that by falling back to whichever row was filled in.
fn security_schemes() -> Value {
    let mut schemes = Map::new();

    for grant in [Grant::Create, Grant::List, Grant::Admin] {
        let scope = if grant.scoped() {
            "Scoped: refused unless the token's `room` claim names the room in the request."
        } else {
            "Unscoped: the token names no room, so it is an authority over every room on this \
             server."
        };

        schemes.insert(
            grant.claim().to_owned(),
            json!({
                "type": "http",
                "scheme": "bearer",
                "bearerFormat": "JWT",
                "description": format!(
                    "A JWT signed with this server's API secret, carrying `video.{}: true`. \
                     {scope}",
                    grant.claim()
                ),
            }),
        );
    }

    Value::Object(schemes)
}

/// The grant an operation insists on, as a security requirement rather than a
/// sentence, so a generated client can enforce it and a reader of the raw
/// document can see it.
fn security(method: &Method) -> Value {
    match method.support {
        Support::Served(grant) => json!([{ grant.claim(): [] }]),
        // Any token this server signed reaches the same 501: the refusal
        // happens before a grant is looked at. Three alternatives, not none --
        // a token is still required, and saying otherwise would have the page
        // send no header and blame the server for the 401.
        Support::Refused(_) => json!([
            { Grant::Create.claim(): [] },
            { Grant::List.claim(): [] },
            { Grant::Admin.claim(): [] },
        ]),
    }
}

fn bodies(schemas: &mut Schemas, message: &MessageDescriptor) -> Value {
    let schema = schemas.reference(message);

    json!({
        "application/json": { "schema": schema },
        "application/protobuf": {
            "schema": {
                "type": "string",
                "format": "binary",
                "description": format!(
                    "The same `{}`, encoded as protobuf. Try it out here speaks JSON; this is \
                     listed because the wire accepts both and answers in whichever was asked.",
                    message.full_name()
                ),
            },
        },
    })
}

/// The statuses one method can actually come back with.
///
/// Not the whole Twirp table on every operation: a caller reading `409` under
/// `DeleteRoom` would go and handle a conflict this server never reports. The
/// full code-to-status table is in the preamble, where it belongs, generated
/// from the same `Code` the server answers with.
fn responses(method: &Method, schemas: &mut Schemas, output: &MessageDescriptor) -> Value {
    let mut responses = Map::new();

    if served(method) {
        responses.insert(
            "200".to_owned(),
            json!({
                "description": format!("`{}`.", output.full_name()),
                "content": bodies(schemas, output),
            }),
        );
        responses.insert(
            "400".to_owned(),
            failure(&[Code::InvalidArgument, Code::Malformed]),
        );
        responses.insert("401".to_owned(), failure(&[Code::Unauthenticated]));
        responses.insert("403".to_owned(), failure(&[Code::PermissionDenied]));
        responses.insert("404".to_owned(), failure(&[Code::NotFound, Code::BadRoute]));
        responses.insert("500".to_owned(), failure(&[Code::Internal]));

        // Only `CreateRoom` refuses a field of a request it otherwise
        // understands, and only because the field promises something this
        // server cannot keep.
        if method.name == "CreateRoom" {
            responses.insert("501".to_owned(), failure(&[Code::Unimplemented]));
        }
    } else {
        responses.insert("401".to_owned(), failure(&[Code::Unauthenticated]));
        responses.insert("404".to_owned(), failure(&[Code::BadRoute]));
        responses.insert("501".to_owned(), failure(&[Code::Unimplemented]));
    }

    Value::Object(responses)
}

fn failure(codes: &[Code]) -> Value {
    let described = codes
        .iter()
        .map(|code| format!("`{}` — {}", code.as_str(), why(*code)))
        .collect::<Vec<String>>()
        .join("; ");

    json!({
        "description": described,
        "content": {
            "application/json": {
                "schema": { "$ref": format!("#/components/schemas/{ERROR_SCHEMA}") },
            },
        },
    })
}

/// What this server means by a code, as opposed to what Twirp means by it.
fn why(code: Code) -> &'static str {
    match code {
        Code::InvalidArgument => "a field the method needs was missing, empty or impossible",
        Code::Malformed => "the body is not the JSON or protobuf it said it was",
        Code::Unauthenticated => {
            "no `Authorization: Bearer` header, or a token this server did not sign, or one that \
             has expired"
        }
        Code::PermissionDenied => {
            "the token is good and does not carry the grant this method wants — or carries \
             `roomAdmin` for another room"
        }
        Code::NotFound => "no such room, participant or track",
        Code::BadRoute => "no such method, or a content type Twirp does not speak, or not a POST",
        Code::Unimplemented => {
            "this server does not implement it, and says so rather than answering as though it had"
        }
        Code::Internal => "a failure on this side; the message says what",
        _ => "",
    }
}

fn summary(method: &Method, comments: &Comments, descriptor: &MethodDescriptor) -> String {
    match method.support {
        Support::Refused(_) => "Not implemented by mediacore".to_owned(),
        Support::Served(_) => comments
            .at(descriptor.parent_file().name(), descriptor.path())
            .map_or_else(|| method.name.to_owned(), first_sentence),
    }
}

fn describe(method: &Method, comments: &Comments, descriptor: &MethodDescriptor) -> String {
    let mut parts = Vec::new();

    if let Support::Refused(reason) = method.support {
        parts.push(format!(
            "**Refused with `unimplemented` (501).** {reason}.\n\nThe path and the messages are \
             real — this is LiveKit's own service — so a client generated from this document \
             compiles against it. The call simply does not work here."
        ));
    }

    if let Some(text) = comments.at(descriptor.parent_file().name(), descriptor.path()) {
        parts.push(text.to_owned());
    }

    if let Support::Served(grant) = method.support {
        let scope = if grant.scoped() {
            " The token's `room` claim has to be the room in the request."
        } else {
            " The grant names no room, so any room on this server is in reach of it."
        };
        parts.push(format!("**Grant:** `{}`.{scope}", grant.claim()));
    }

    if !method.requires.is_empty() {
        let named = method
            .requires
            .iter()
            .map(|field| format!("`{field}`"))
            .collect::<Vec<String>>()
            .join(", ");
        parts.push(format!(
            "**Required:** {named}. Missing or empty and the call comes back \
             `invalid_argument`, naming the field, having done nothing."
        ));
    }

    if !method.note.is_empty() {
        parts.push(format!("**On this server.** {}", method.note));
    }

    if method.name == "CreateRoom" {
        parts.push(create_room_fields());
    }

    parts.join("\n\n")
}

/// Field by field, what `CreateRoom` does with a request.
///
/// Rendered from the three lists in `rooms`, which a test holds against the
/// descriptor: between them they name every field `CreateRoomRequest` has. So
/// this table cannot go stale by omission -- a field LiveKit adds fails a test
/// until somebody decides which column it belongs in.
fn create_room_fields() -> String {
    let mut table = String::from(
        "**Every field of the request, and what happens to it.**\n\n\
         | Field | Treatment | Detail |\n|---|---|---|\n",
    );

    for (field, what) in rooms::CREATE_HONOURED {
        table.push_str(&format!("| `{field}` | honoured | {what} |\n"));
    }
    for (field, reason) in rooms::CREATE_IGNORED {
        table.push_str(&format!(
            "| `{field}` | accepted and ignored | {reason} |\n"
        ));
    }
    for (field, reason) in rooms::CREATE_REFUSED {
        table.push_str(&format!("| `{field}` | refused | {reason} |\n"));
    }

    table.push_str(
        "\nA refused field refuses the whole call with `unimplemented`, naming every field it \
         could not honour, and creates no room. The line is between a promise about the meeting \
         — a recording that will exist, an agent that will join, a tag that can be read back — \
         and a hint about node placement or playout timing, which costs nothing to ignore on a \
         one-node server.",
    );

    table
}

/// The page's opening: what this API is, what a token has to carry, what the
/// Try-it-out button will really do, and how protobuf JSON differs from the
/// JSON a reader expects.
fn preamble() -> String {
    let mut text = String::from(
        "`mediacore.RoomService`: the API a backoffice uses to create rooms, moderate them and \
         take them away again. It is LiveKit's room service, method for method, under \
         mediacore's own protobuf package.\n\n\
         It is [Twirp](https://twitchtv.github.io/twirp/docs/spec_v7.html), not REST. Every \
         method is a `POST` to `/twirp/mediacore.RoomService/<Method>` with one request body and \
         one response body — no path parameters, no query strings, no other verbs. A `GET` to \
         one of these paths is not a 405, it is `bad_route`, because Twirp identifies a call by \
         its path *and* its content type together.\n\n",
    );

    text.push_str(
        "Unless the deployment has set `twirp.livekit_alias: false`, every method also answers at \
         `/twirp/livekit.RoomService/<Method>` — the path LiveKit's own server SDKs and `lk` \
         build in generated code and cannot be pointed away from. It is the same handler, the \
         same grants and the same error bodies; only a `bad_route` message names whichever \
         service you posted to. Where it has been retired that path is not routed at all, so a \
         call to it comes back as a bare 404 with no Twirp code in it.\n\n",
    );

    text.push_str(
        "## What happened, as opposed to what is happening\n\n\
         Three endpoints under **Sessions** below are not Twirp and not part of this service: \
         plain `GET`s at `/api/rooms/<room>/sessions`, `/api/rooms/<room>/session` and \
         `/api/sessions/<id>`. A *session* is one meeting; a *room* is the place it happened in, \
         which outlives it and holds the next one. They are here because this page is where \
         somebody integrating against this server looks, and an endpoint nobody can find is an \
         endpoint nobody calls.\n\n",
    );

    text.push_str(
        "## Try it out is not a sandbox\n\n\
         Every button on this page sends a real call to the server that served the page. \
         `DeleteRoom` really deletes the room and hangs up on everybody in it. \
         `RemoveParticipant` really removes somebody, `MutePublishedTrack` really mutes them, \
         and `UpdateParticipant` can really take away somebody's microphone. Nothing here is a \
         dry run and nothing is undone.\n\n",
    );

    text.push_str(
        "## What a token needs\n\n\
         Every call wants `Authorization: Bearer <jwt>`, signed with this server's API secret — \
         a **server** token, not the join token a browser gets. A join token administers \
         nothing: `roomJoin` is the grant everybody in the meeting holds, and it must not be \
         able to end the meeting.\n\n",
    );

    text.push_str("| Grant | Reach | Methods |\n|---|---|---|\n");
    for grant in [Grant::Create, Grant::List, Grant::Admin] {
        let methods = rooms::METHODS
            .iter()
            .filter(|method| matches!(method.support, Support::Served(held) if held == grant))
            .map(|method| format!("`{}`", method.name))
            .collect::<Vec<String>>()
            .join(", ");
        let reach = if grant.scoped() {
            "the one room named in the token's `room` claim"
        } else {
            "**every room on the server**"
        };

        text.push_str(&format!("| `{}` | {reach} | {methods} |\n", grant.claim()));
    }

    text.push_str(
        "\n`roomAdmin` is the only scoped grant. `roomCreate` names no room and also gates \
         `DeleteRoom`, so a token that may open a meeting may end any meeting in progress; \
         `roomList` sees every room and the whole of every record, metadata included. An API \
         token is a server credential, not a per-meeting one — mint it for a backoffice, keep \
         it off anything a browser can read, and hand out `roomAdmin` where somebody needs \
         power over one meeting.\n\n\
         Press **Authorize**, and paste the token into the row for the grant it carries. If you \
         then call a method wanting a grant you left blank, this page sends the token you did \
         fill in rather than no header at all — so the answer you get is this server's judgement \
         of your token, not a 401 you have to decode.\n\n",
    );

    text.push_str(
        "## The JSON is protobuf's\n\n\
         Bodies are [protobuf JSON](https://protobuf.dev/programming-guides/json/), generated \
         from the same `.proto` as the wire types, which is not quite the JSON a reader \
         expects:\n\n\
         - **64-bit integers travel as strings.** `creationTime` is `\"1717430400\"`, not a \
           number, because a double cannot hold every `int64`.\n\
         - **Fields at their default are left out of a response.** No `numParticipants` means \
           zero, not unknown.\n\
         - **Enums are their name**, `\"AUDIO\"` rather than `0`. A number is accepted on the \
           way in.\n\
         - **`bytes` is base64.**\n\
         - **A field name we do not know is refused**, rather than dropped; both `agentName` and \
           `agent_name` are recognised.\n\n",
    );

    text.push_str(
        "## Errors\n\n\
         A failure is `{\"code\": \"...\", \"msg\": \"...\"}` with the status Twirp gives that \
         code — which is what an SDK turns back into a typed error, and what every proxy and \
         retry policy in between reacts to.\n\n\
         | Code | Status |\n|---|---|\n",
    );
    for code in Code::ALL {
        text.push_str(&format!(
            "| `{}` | {} |\n",
            code.as_str(),
            code.status().as_u16()
        ));
    }
    text.push_str(
        "\nThe eight this service actually answers with are on the operations below. One \
         deliberate difference from LiveKit: a valid token without the grant is \
         `permission_denied` (403) here, where LiveKit answers `unauthenticated` (401). Neither \
         is retryable, so no SDK behaves differently, and 403 is the honest answer to a token \
         that was read and understood.",
    );

    text
}

/// One session endpoint, from the same table the router reads.
fn session_path(endpoint: &Endpoint) -> Value {
    let mut operation = Map::new();
    operation.insert("operationId".to_owned(), json!(endpoint.operation));
    operation.insert("summary".to_owned(), json!(endpoint.summary));
    operation.insert(
        "description".to_owned(),
        json!(session_description(endpoint)),
    );
    operation.insert("tags".to_owned(), json!([SESSIONS_TAG]));
    // Either grant, which is what a list of two requirements means in OpenAPI.
    // `roomList` reads every room on the server; `roomAdmin` reads the one its
    // token names.
    operation.insert(
        "security".to_owned(),
        json!([
            { Grant::List.claim(): [] },
            { Grant::Admin.claim(): [] },
        ]),
    );
    operation.insert("x-mediacore-implemented".to_owned(), json!(true));
    let mut parameters = vec![json!({
        "name": endpoint.parameter.0,
        "in": "path",
        "required": true,
        "description": endpoint.parameter.1,
        "schema": { "type": "string" },
    })];
    // Only the list pages, because only the list can be long. The other two
    // answer with one meeting.
    if endpoint.answer == Answer::List {
        parameters.push(json!({
            "name": "limit",
            "in": "query",
            "required": false,
            "description": format!(
                "How many meetings this page holds. {DEFAULT_PAGE} by default, {MAX_PAGE} at \
                 most; a larger number is capped rather than refused."
            ),
            "schema": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE, "default": DEFAULT_PAGE },
        }));
        parameters.push(json!({
            "name": "offset",
            "in": "query",
            "required": false,
            "description":
                "How many meetings to skip, counting back from the most recent. Read `total` off \
                 the answer to know when there are no more.",
            "schema": { "type": "integer", "minimum": 0, "default": 0 },
        }));
    }
    operation.insert("parameters".to_owned(), json!(parameters));

    let mut responses = Map::new();
    responses.insert(
        "200".to_owned(),
        json!({
            "description": session_answer_description(endpoint.answer),
            "content": { "application/json": { "schema": session_answer(endpoint.answer) } },
        }),
    );
    responses.insert("401".to_owned(), failure(&[Code::Unauthenticated]));
    if endpoint.answer == Answer::One {
        responses.insert("404".to_owned(), failure(&[Code::NotFound]));
    } else {
        responses.insert("403".to_owned(), failure(&[Code::PermissionDenied]));
    }
    responses.insert("500".to_owned(), failure(&[Code::Internal]));
    operation.insert("responses".to_owned(), Value::Object(responses));

    json!({ "get": Value::Object(operation) })
}

fn session_description(endpoint: &Endpoint) -> String {
    let scope = match endpoint.answer {
        Answer::One => {
            "**Grant:** `roomList`, or `roomAdmin` whose `room` claim is the room the meeting              happened in. A meeting the token may not read answers `not_found`, identically to              an id that names nothing, so this cannot be used to find out which meetings exist              elsewhere."
        }
        _ => {
            "**Grant:** `roomList`, or `roomAdmin` whose `room` claim is the room in the path.              Anything else is `permission_denied`."
        }
    };

    format!(
        "{}\n\n{scope}\n\nTimes are milliseconds since the epoch, as JSON numbers. This reads          the record rather than the registry, so a room that no longer exists still answers for          the meetings it held.",
        endpoint.description
    )
}

fn session_answer(answer: Answer) -> Value {
    let session = json!({ "$ref": format!("#/components/schemas/{SESSION_SCHEMA}") });

    match answer {
        Answer::List => json!({
            "type": "object",
            "required": ["sessions", "total"],
            "properties": {
                "sessions": { "type": "array", "items": session },
                "total": {
                    "type": "integer",
                    "description":
                        "How many meetings the room has held altogether, of which this page is a \
                         part.",
                },
            },
        }),
        Answer::Perhaps => json!({
            "type": "object",
            "required": ["session"],
            "properties": { "session": { "nullable": true, "allOf": [session] } },
        }),
        Answer::One => json!({
            "type": "object",
            "required": ["session"],
            "properties": { "session": session },
        }),
    }
}

fn session_answer_description(answer: Answer) -> &'static str {
    match answer {
        Answer::List => {
            "One page of the room's meetings, newest first, with `total` saying how many                          there were. An empty list for a room that has never held one."
        }
        Answer::Perhaps => {
            "The meeting running now, or `null`. No meeting is an answer and not a                             failure, so this is 200 either way."
        }
        Answer::One => "The meeting.",
    }
}

fn session_schema() -> Value {
    json!({
        "type": "object",
        "title": "Session",
        "description":
            "One meeting held in one room. A room created over the room service outlives its              meetings and holds many; a room that exists only because somebody walked into it is              gone when they leave, and its meeting is still recorded. The room is named rather              than identified by sid, because a meeting that survives a blink spans two rooms with              two sids and one name.",
        "required": ["id", "room", "startedAtMs", "participants"],
        "properties": {
            "id": {
                "type": "string",
                "description":
                    "`SE_` and twelve hex digits. What a recording or a transcript hangs off                      later, because those belong to a meeting and not to a room.",
            },
            "room": { "type": "string", "description": "The room's name." },
            "startedAtMs": {
                "type": "integer",
                "format": "int64",
                "description":
                    "When the room reached the number of people it needs -- one, unless the room                      carries a `sessionPolicy` asking for more.",
            },
            "endedAtMs": {
                "type": "integer",
                "format": "int64",
                "nullable": true,
                "description":
                    "When the meeting ended, or null while it is running. A meeting ends when the                      room falls below its floor and the grace period runs out, which is why this                      can be later than the last person's `leftAtMs`.",
            },
            "endedBy": {
                "type": "string",
                "nullable": true,
                "enum": ["grace", "closed", "restart"],
                "description":
                    "`grace`: nobody came back before the grace period ran out, the ordinary end                      of a meeting. `closed`: the room was deleted under it. `restart`: the server                      was still holding this meeting when it stopped, so `endedAtMs` is the last                      moment the record knows about rather than when the meeting really ended.",
            },
            "participants": {
                "type": "array",
                "items": { "$ref": format!("#/components/schemas/{ATTENDANCE_SCHEMA}") },
                "description":
                    "Every stretch of attendance, oldest first. One entry per stretch and not per                      person: somebody who dropped out and came back is two.",
            },
        },
    })
}

fn attendance_schema() -> Value {
    json!({
        "type": "object",
        "title": "Attendance",
        "description": "One stretch of one connection being at one meeting.",
        "required": ["sid", "identity", "kind", "joinedAtMs"],
        "properties": {
            "sid": {
                "type": "string",
                "description":
                    "The connection, as `PA_` and twelve hex digits -- the same sid the roster                      and the server log carry.",
            },
            "identity": {
                "type": "string",
                "description":
                    "The identity the room used, suffix included: a person's second connection is                      `alice#2`.",
            },
            "name": { "type": "string", "description": "The display name, or empty." },
            "kind": {
                "type": "string",
                "enum": ["standard", "egress", "agent"],
                "description":
                    "A person, a recorder or a bot. Only `standard` counts towards the number of                      people a room needs, which is why a room of recorders is not a meeting.",
            },
            "joinedAtMs": {
                "type": "integer",
                "format": "int64",
                "description":
                    "When they arrived. Can be earlier than `startedAtMs`: the first person into                      a room with a floor of two was waiting for the second.",
            },
            "leftAtMs": {
                "type": "integer",
                "format": "int64",
                "nullable": true,
                "description": "When they left, or null while they are still here.",
            },
        },
    })
}

fn error_schema() -> Value {
    let answered = [
        Code::InvalidArgument,
        Code::Malformed,
        Code::Unauthenticated,
        Code::PermissionDenied,
        Code::NotFound,
        Code::BadRoute,
        Code::Unimplemented,
        Code::Internal,
    ];

    json!({
        "type": "object",
        "title": "Twirp error",
        "description":
            "The body of every failure, whatever the request was encoded as: an error is always \
             JSON, because a caller we could not understand cannot be assumed to understand a \
             protobuf answer either.",
        "required": ["code", "msg"],
        "properties": {
            "code": {
                "type": "string",
                "enum": answered.iter().map(|code| code.as_str()).collect::<Vec<&str>>(),
                "description": "The Twirp code. Its HTTP status is fixed by the specification.",
            },
            "msg": {
                "type": "string",
                "description":
                    "What went wrong, in a sentence meant for whoever is reading the log.",
            },
        },
    })
}

/// The leading comments in the `.proto`, keyed the way protobuf keys them: the
/// file the declaration is in, and its path within that file.
///
/// This is why the descriptor set is kept with `source_code_info` in it. The
/// comments beside an rpc and beside a field are the documentation LiveKit
/// already wrote; a page listing field names with no idea what they mean is a
/// schema dump.
struct Comments(HashMap<(String, Vec<i32>), String>);

impl Comments {
    fn read(pool: &DescriptorPool) -> Self {
        let mut found = HashMap::new();

        for file in pool.files() {
            let proto = file.file_descriptor_proto();
            let Some(info) = proto.source_code_info.as_ref() else {
                continue;
            };

            for location in &info.location {
                let Some(raw) = location.leading_comments.as_deref() else {
                    continue;
                };
                let text = tidy(raw);
                if !text.is_empty() {
                    found.insert((proto.name().to_owned(), location.path.clone()), text);
                }
            }
        }

        Self(found)
    }

    fn at(&self, file: &str, path: &[i32]) -> Option<&str> {
        self.0
            .get(&(file.to_owned(), path.to_vec()))
            .map(String::as_str)
    }

    fn message(&self, message: &MessageDescriptor) -> Option<&str> {
        self.at(message.parent_file().name(), message.path())
    }

    fn field(&self, field: &FieldDescriptor) -> Option<&str> {
        self.at(field.parent_file().name(), field.path())
    }

    fn enumeration(&self, enumeration: &EnumDescriptor) -> Option<&str> {
        self.at(enumeration.parent_file().name(), enumeration.path())
    }
}

/// A protobuf comment is every line with one leading space stripped.
///
/// Newlines survive: a comment that was a bulleted list stays one, because
/// these are rendered as Markdown. What does not survive is a comment that is
/// only a `NEXT_ID` marker or a deprecation note to the LiveKit team, and
/// those are left alone rather than filtered -- guessing which comments are
/// for us is how a generator starts dropping the useful ones.
fn tidy(raw: &str) -> String {
    raw.lines()
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<&str>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn first_sentence(text: &str) -> String {
    let flat = text.replace('\n', " ");
    match flat.find(". ") {
        Some(end) => flat[..=end].trim().to_owned(),
        None => flat.trim().to_owned(),
    }
}

/// Every message and enum the documented methods reach, as OpenAPI schemas.
struct Schemas<'a> {
    comments: &'a Comments,
    out: BTreeMap<String, Value>,
}

impl<'a> Schemas<'a> {
    fn new(comments: &'a Comments) -> Self {
        Self {
            comments,
            out: BTreeMap::new(),
        }
    }

    fn finish(self) -> Map<String, Value> {
        self.out.into_iter().collect()
    }

    /// A `$ref` to a message, generating it the first time it is asked for.
    ///
    /// The name is reserved before the body is built, because a message can
    /// reach itself -- and a generator that recursed into one would not stop.
    fn reference(&mut self, message: &MessageDescriptor) -> Value {
        let name = message.full_name().to_owned();

        if !self.out.contains_key(&name) {
            self.out.insert(name.clone(), Value::Null);
            let schema = self.message(message);
            self.out.insert(name.clone(), schema);
        }

        json!({ "$ref": format!("#/components/schemas/{name}") })
    }

    fn message(&mut self, message: &MessageDescriptor) -> Value {
        let mut properties = Map::new();

        for field in message.fields() {
            properties.insert(field.json_name().to_owned(), self.field(&field));
        }

        let mut schema = Map::new();
        schema.insert("type".to_owned(), json!("object"));
        schema.insert("title".to_owned(), json!(message.full_name()));
        if let Some(text) = self.comments.message(message) {
            schema.insert("description".to_owned(), json!(text));
        }
        schema.insert("properties".to_owned(), Value::Object(properties));

        Value::Object(schema)
    }

    fn field(&mut self, field: &FieldDescriptor) -> Value {
        let schema = if field.is_map() {
            let value = field
                .kind()
                .as_message()
                .expect("a map field's kind is its entry message")
                .map_entry_value_field();

            json!({
                "type": "object",
                "additionalProperties": self.kind(&value),
            })
        } else if field.is_list() {
            json!({
                "type": "array",
                "items": self.kind(field),
            })
        } else {
            self.kind(field)
        };

        match self.comments.field(field) {
            Some(text) => with_description(schema, text),
            None => schema,
        }
    }

    /// One protobuf type as JSON sees it.
    ///
    /// The two that surprise people are here: a 64-bit integer is a *string*
    /// in protobuf JSON, because a JSON number is a double and cannot carry
    /// every `int64`; and `bytes` is base64 in a string. Getting either wrong
    /// on this page would have a caller write a client that fails on the first
    /// room old enough to matter.
    fn kind(&mut self, field: &FieldDescriptor) -> Value {
        match field.kind() {
            Kind::Double => json!({ "type": "number", "format": "double" }),
            Kind::Float => json!({ "type": "number", "format": "float" }),
            Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
                json!({ "type": "integer", "format": "int32" })
            }
            Kind::Uint32 | Kind::Fixed32 => {
                json!({ "type": "integer", "format": "int64", "minimum": 0 })
            }
            Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => int64("a signed 64-bit integer"),
            Kind::Uint64 | Kind::Fixed64 => int64("an unsigned 64-bit integer"),
            Kind::Bool => json!({ "type": "boolean" }),
            Kind::String => json!({ "type": "string" }),
            Kind::Bytes => json!({
                "type": "string",
                "format": "byte",
                "description": "Base64, standard alphabet with padding.",
            }),
            Kind::Message(message) => match message.full_name() {
                "google.protobuf.Timestamp" => json!({
                    "type": "string",
                    "format": "date-time",
                    "description": "RFC 3339, in UTC: `2026-09-06T09:14:00Z`.",
                }),
                "google.protobuf.Duration" => json!({
                    "type": "string",
                    "description": "Seconds with a `s` suffix: `1.5s`.",
                }),
                _ => self.reference(&message),
            },
            Kind::Enum(enumeration) => self.enumeration(&enumeration),
        }
    }

    fn enumeration(&mut self, enumeration: &EnumDescriptor) -> Value {
        let name = enumeration.full_name().to_owned();

        if !self.out.contains_key(&name) {
            let values = enumeration
                .values()
                .map(|value| value.name().to_owned())
                .collect::<Vec<String>>();
            let mut schema = Map::new();
            schema.insert("type".to_owned(), json!("string"));
            schema.insert("title".to_owned(), json!(name.clone()));
            schema.insert("enum".to_owned(), json!(values));
            let mut description = String::from(
                "Written as the name. The number is accepted on the way in and never sent back.",
            );
            if let Some(text) = self.comments.enumeration(enumeration) {
                description = format!("{text}\n\n{description}");
            }
            schema.insert("description".to_owned(), json!(description));

            self.out.insert(name.clone(), Value::Object(schema));
        }

        json!({ "$ref": format!("#/components/schemas/{name}") })
    }
}

fn int64(what: &str) -> Value {
    json!({
        "type": "string",
        "format": "int64",
        "description": format!("{what}, written as a string — a JSON number is a double."),
    })
}

/// Hangs a description on a schema, going through `allOf` when the schema is a
/// bare `$ref`.
///
/// OpenAPI 3.0 ignores everything beside a `$ref`, so a field's comment would
/// simply vanish for every field whose type is a message or an enum -- which
/// is most of the interesting ones.
fn with_description(schema: Value, text: &str) -> Value {
    if schema.get("$ref").is_some() {
        return json!({
            "allOf": [schema],
            "description": text,
        });
    }

    let mut schema = schema;
    if let Value::Object(fields) = &mut schema {
        match fields.get("description").and_then(Value::as_str) {
            Some(existing) => {
                let merged = format!("{text}\n\n{existing}");
                fields.insert("description".to_owned(), json!(merged));
            }
            None => {
                fields.insert("description".to_owned(), json!(text));
            }
        }
    }

    schema
}

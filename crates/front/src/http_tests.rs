
use super::*;

const README_MD: &str = include_str!("../README.md");

#[test]
fn the_sdk_route_carries_the_vendored_version() {
    assert!(
        SDK_PATH.contains(SDK_VERSION),
        "{SDK_PATH} does not name version {SDK_VERSION}, so a browser would reuse a cached copy of another build"
    );
    assert!(
        APP_JS.contains(SDK_PATH),
        "app.js does not import the sdk from {SDK_PATH}"
    );
}

#[test]
fn every_module_the_page_imports_is_compiled_in() {
    let served = [
        ("/app.js", APP_JS),
        ("/bench.js", BENCH_JS),
        ("/readout.js", READOUT_JS),
        ("/stage.js", STAGE_JS),
        ("/room.js", ROOM_JS),
        ("/chat.js", CHAT_JS),
        ("/prefs.js", PREFS_JS),
        ("/media.js", MEDIA_JS),
        ("/voice.js", VOICE_JS),
        (LEVEL_METER_PATH, LEVEL_METER_JS),
        ("/denoise.js", DENOISE_JS),
        (DENOISE_GATE_PATH, DENOISE_GATE_JS),
    ];

    for (path, module) in served {
        for import in imports(module) {
            assert!(
                served.iter().any(|(candidate, _)| *candidate == import),
                "{path} imports {import}, which this server does not serve"
            );
        }
    }

    for (path, module) in [
        ("/bench.js", BENCH_JS),
        ("/stage.js", STAGE_JS),
        ("/room.js", ROOM_JS),
        ("/chat.js", CHAT_JS),
        ("/prefs.js", PREFS_JS),
        ("/voice.js", VOICE_JS),
        (LEVEL_METER_PATH, LEVEL_METER_JS),
    ] {
        assert!(
            imports(module).is_empty(),
            "{path} is meant to be a leaf of the module graph, so the behaviour suite can \
                 load it off disk where the page's absolute paths do not resolve"
        );
    }
}

fn imports(module: &str) -> Vec<String> {
    module
        .match_indices("from \"")
        .filter_map(|(index, marker)| {
            let rest = &module[index + marker.len()..];
            rest.split_once('"').map(|(path, _)| path.to_owned())
        })
        .filter(|path| path.ends_with(".js") && !path.contains("/vendor/"))
        .map(|path| match path.strip_prefix("./") {
            Some(rest) => format!("/{rest}"),
            None => path,
        })
        .collect()
}

#[test]
fn the_grant_covers_a_screen_share_and_stops_there() {
    assert!(
        PUBLISH_SOURCES.contains(&"screen_share"),
        "the server refuses a screen share the token does not name"
    );
    assert!(
        PUBLISH_SOURCES.contains(&"screen_share_audio"),
        "the screen would be shared without the sound the browser offered with it"
    );

    for source in PUBLISH_SOURCES {
        assert!(
            ["microphone", "camera", "screen_share", "screen_share_audio"].contains(&source),
            "`{source}` is granted but nothing on the page publishes it"
        );
    }
}

#[test]
fn the_policy_names_the_signalling_origin_and_forbids_the_rest() {
    let policy = content_security_policy("wss://media.example.test");
    let policy = policy.to_str().unwrap();

    assert!(policy.starts_with("default-src 'none';"));
    assert!(policy.contains("script-src 'self' 'wasm-unsafe-eval';"));
    assert!(
        policy.contains("connect-src 'self' wss://media.example.test https://media.example.test;")
    );
    assert!(policy.contains("frame-ancestors 'none'"));
}

#[test]
fn the_policy_buys_wasm_and_nothing_else() {
    let policy = content_security_policy("wss://media.example.test");
    let policy = policy.to_str().unwrap();

    assert!(
        policy.contains("'wasm-unsafe-eval'"),
        "chrome refuses to compile the denoiser without it, so the feature would be dead on \
             arrival: {policy}"
    );
    assert!(
        !policy.contains("'unsafe-eval'"),
        "the policy bought wasm with a general eval: {policy}"
    );
    assert!(
        !policy.contains("'unsafe-inline'"),
        "the policy gained inline scripts: {policy}"
    );
    assert!(
        !policy.contains("script-src 'self' 'wasm-unsafe-eval' "),
        "a source other than this origin was added to script-src: {policy}"
    );
    assert!(
        policy.contains("connect-src 'self' wss://media.example.test https://media.example.test;"),
        "connect-src names something beyond the signalling origin: {policy}"
    );
    assert!(policy.contains("default-src 'none';"));
}

#[test]
fn the_denoiser_routes_carry_the_vendored_version() {
    for path in [
        RNNOISE_WASM_PATH,
        RNNOISE_SIMD_WASM_PATH,
        RNNOISE_WORKLET_PATH,
    ] {
        assert!(
            path.contains(RNNOISE_VERSION),
            "{path} does not name version {RNNOISE_VERSION}, so an upgrade could be served \
                 from a browser's cache of the old build"
        );
        assert!(
            APP_JS.contains(path),
            "nothing on the page loads {path}, so the route is dead"
        );
    }

    assert!(
        APP_JS.contains(DENOISE_PROBE_PATH),
        "the page never loads the probe worklet, so nothing measures the node"
    );
    assert!(
        APP_JS.contains(DENOISE_GATE_PATH),
        "the page never loads the gate worklet, so the vendored node would publish silence              until its wasm compiled, and for ever if it never did"
    );
}

#[test]
fn the_denoiser_is_served_by_this_binary_and_not_fetched_from_anywhere() {
    assert!(
        !RNNOISE_WASM.is_empty() && !RNNOISE_SIMD_WASM.is_empty(),
        "a denoiser route would answer with nothing"
    );
    assert_eq!(
        &RNNOISE_WASM[..4],
        b"\0asm",
        "the plain build is not a WebAssembly module"
    );
    assert_eq!(
        &RNNOISE_SIMD_WASM[..4],
        b"\0asm",
        "the simd build is not a WebAssembly module"
    );

    for (name, module) in [
        ("denoise.js", DENOISE_JS),
        ("denoise-gate.js", DENOISE_GATE_JS),
        ("denoise-probe.js", DENOISE_PROBE_JS),
        ("app.js", APP_JS),
        ("chat.js", CHAT_JS),
        ("prefs.js", PREFS_JS),
        ("voice.js", VOICE_JS),
        ("level-meter.js", LEVEL_METER_JS),
    ] {
        for host in ["cdn.jsdelivr.net", "unpkg.com", "cdn.mezon.ai", "https://"] {
            assert!(
                !module.contains(host),
                "{name} names {host}, so something would be fetched from off this origin"
            );
        }
    }
}

/// Apache-2.0 section 4(d) and the MIT permission notice both require the
/// notice to travel with the thing they cover. The file existing in the
/// tree is not that: it has to be inside the binary a container runs, and
/// reachable from the page.
#[test]
fn the_licence_notice_travels_with_the_binary() {
    assert!(
        NOTICE_TXT.len() > 4096,
        "the notice is too short to be carrying the licence texts it has to carry"
    );

    for project in [
        "livekit-client",
        "@sapphi-red/web-noise-suppressor",
        "@shiguredo/rnnoise-wasm",
        "xiph/rnnoise",
    ] {
        assert!(
            NOTICE_TXT.contains(project),
            "{project} is served by this binary and is not named in the notice"
        );
    }

    for licence in [
        "Apache License",
        "MIT License",
        "Redistribution and use in source and binary forms",
    ] {
        assert!(
            NOTICE_TXT.contains(licence),
            "the notice names a project under {licence} without reproducing it"
        );
    }

    assert!(
        INDEX_HTML.contains(NOTICE_PATH),
        "nothing on the page links to {NOTICE_PATH}, so nobody who runs this ever sees it"
    );
}

/// The README quotes what the denoiser costs a page load, down to the byte,
/// and a figure like that goes stale the first time somebody edits a file.
/// These are the bytes this binary serves; if they no longer match the ones
/// written down, the ones written down are wrong.
#[test]
fn the_page_weight_in_the_readme_is_the_page_weight() {
    let every_load = [
        (RNNOISE_SIMD_WASM_PATH, RNNOISE_SIMD_WASM.len()),
        ("/denoise.js", DENOISE_JS.len()),
    ];
    let on_demand = [
        (RNNOISE_WORKLET_PATH, RNNOISE_WORKLET_JS.len()),
        (DENOISE_PROBE_PATH, DENOISE_PROBE_JS.len()),
        (DENOISE_GATE_PATH, DENOISE_GATE_JS.len()),
    ];

    for (path, bytes) in every_load.iter().chain(on_demand.iter()) {
        let row = format!("| `{path}` | {} |", grouped(*bytes));
        assert!(
            README_MD.contains(&row),
            "the README does not carry `{row}`, so it is quoting a page weight that is no \
                 longer what this binary serves"
        );
    }

    let first: usize = every_load.iter().map(|(_, bytes)| bytes).sum();
    let total: usize = first + on_demand.iter().map(|(_, bytes)| bytes).sum::<usize>();

    for (label, bytes) in [("every load", first), ("once the node goes in", total)] {
        let row = format!("| **{label}** | **{}** |", grouped(bytes));
        assert!(
            README_MD.contains(&row),
            "the README does not carry `{row}`"
        );
    }

    for (what, bytes) in [
        ("the plain wasm build", RNNOISE_WASM.len()),
        ("the sdk", SDK_JS.len()),
    ] {
        assert!(
            README_MD.contains(&grouped(bytes)),
            "the README does not name the {what} at {} bytes",
            grouped(bytes)
        );
    }
}

/// Thousands separated by a space, which is how the README writes them.
fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(digit);
    }
    out
}

/// The Apache-2.0 build is the one whose bytes are actually shipped, and it
/// is the one that was missing from the notice. Its own banner is inside
/// the vendored worklet, so the two can be checked against each other.
#[test]
fn the_notice_names_the_project_the_wasm_came_from() {
    assert!(
        RNNOISE_WORKLET_JS.contains("@shiguredo/rnnoise-wasm"),
        "the vendored worklet no longer carries the banner the notice was written against;              check what the wasm is now built from before touching the notice"
    );
    assert!(
        RNNOISE_WORKLET_JS.contains("@license: Apache-2.0"),
        "the vendored worklet no longer declares Apache-2.0"
    );
    assert!(
        NOTICE_TXT.contains("@shiguredo/rnnoise-wasm") && NOTICE_TXT.contains("Apache License"),
        "the wasm this binary serves is Apache-2.0 and the notice does not carry that licence"
    );
}

#[test]
fn the_policy_allows_the_http_origin_the_sdk_validates_against() {
    let insecure = content_security_policy("ws://127.0.0.1:7880");
    let insecure = insecure.to_str().unwrap();

    assert!(
        insecure.contains("ws://127.0.0.1:7880 http://127.0.0.1:7880"),
        "livekit-client fetches /rtc/validate over http before it opens the socket, and a \
             policy that names only the socket blocks the join: {insecure}"
    );

    let secure = content_security_policy("wss://media.example.test");
    let secure = secure.to_str().unwrap();

    assert!(secure.contains("wss://media.example.test https://media.example.test"));
    assert!(
        !secure.contains("http://media.example.test"),
        "a secure deployment must not be told to allow plain http: {secure}"
    );
}

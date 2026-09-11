//! `mediacore check`: whether a config is the one the deployment meant.
//!
//! **Three questions this server cannot ask itself.**
//! [`mediacore_config::Config::validate`] is about what a mediacore can run
//! with, and everything in it is true of every deployment there will ever be: a
//! nameless api key, a secret too short to sign with, a lease shorter than
//! three statement timeouts. The three below are not like that. Each is a
//! statement of intent only the deployment can make, and each has a shape the
//! server is right to accept:
//!
//! - `rtc.advertise_addresses: [127.0.0.1]` is what
//!   `deploy/docker-compose.local.yml` says deliberately, and a blanket refusal
//!   would take the local trial away with it. Under a chart the same value is a
//!   node address that was never filled in, and every call connects, shows
//!   everybody's name and carries no sound.
//! - An empty `sessions.postgres.url` means "no database, keep the meetings in
//!   a directory", which is a deployment somebody runs on purpose. It is also
//!   what a Secret with a missing key renders as, and then it is every meeting
//!   record moved onto a path that may not be a volume at all, with nothing
//!   said about it anywhere.
//! - Recording off and no bucket named is how a deployment that records nothing
//!   is written. On one that meant to record, no recorder is built at all: a
//!   room that asks to be recorded in its own metadata simply is not, nothing
//!   is refused, and the meeting is found to be missing when somebody asks for
//!   it a month later.
//!
//! So the deployment says which of them it meant, in flags, and this refuses
//! what does not match. `recording.enabled` is deliberately not one of the
//! checks: it is the default for rooms rather than a master switch, and a
//! deployment that records only the rooms asking for it is an ordinary one.
//!
//! **And one refusal no flag asks for**, which is [`listen`]: `bind_addresses`
//! is a list `Config::validate` counts and never reads, so an interface or a
//! host name in it is a config this server accepts and a process that exits on
//! `invalid socket address syntax` with the setting unnamed. Nothing about that
//! is a statement of intent -- nobody meant `bind_addresses: [eth0]` -- so it is
//! asked of every `check`, flags or none.
//!
//! **Why it is a subcommand of this binary.** The same reason `healthcheck` is.
//! These checks are `/bin/sh -c` in the chart's init container today, and that
//! shell is the last thing keeping the published image on Debian -- the
//! distroless runtime is built and proven on every CI run and cannot ship while
//! something in the deployment needs a shell out of the same image. This is the
//! half of that swap which lives in the repository; the chart moves to it
//! separately, and until it does nothing here is load-bearing anywhere.
//! Everything the shell does is either the server's own by now (it fills in its
//! `${MEDIACORE_*}` and validates what comes out) or this, which reuses that
//! whole path rather than reading the file a second way: a check that parsed
//! the config differently from the server would be a check that passes on a
//! config the server then refuses.
//!
//! **Every refusal at once**, for the reason [`mediacore_config::variables`]
//! collects missing names rather than raising the first: a deployment that
//! forgot its database url has very likely forgotten the bucket beside it, and
//! one name per restart is one outage per name.
//!
//! **The message is the product.** Nothing here is meant to be parsed. What an
//! init container leaves behind is what somebody reads with `kubectl logs`
//! after a pod would not start, so each refusal says what is wrong, why nothing
//! else would have noticed, and which variable fixes it. The exit code is the
//! only other thing it says.

use std::net::{IpAddr, SocketAddr};

use mediacore_config::{Config, RtcConfig};

/// What the deployment states it asked for.
///
/// All three default to false, and a `check` with none of them set is still
/// worth running: it reads the file, fills in every `${MEDIACORE_*}` and runs
/// the server's own validation, so an unset variable, a nameless key or a
/// secret too short to sign with is one message from an init container that
/// ran once instead of a container that restarts for ever.
#[derive(Debug, Clone, Copy, Default)]
pub struct Expected {
    /// Meetings belong in Postgres, so an empty url is a refusal.
    pub postgres: bool,
    /// Meetings are recorded, so the object store has to be there.
    pub recording: bool,
    /// The advertised media address has to be one a client off this node can
    /// send to.
    pub routable_advertise: bool,
}

/// What one check has to say: the line to print if it holds, or why it does
/// not.
///
/// A `String` rather than an `anyhow::Error` because every one of these is
/// collected before any is raised, and the first thing an error would be asked
/// for is its message.
type Answer = Result<String, String>;

/// Read the config the way the server reads it, then judge it against what the
/// deployment said it wanted.
pub fn run(path: &str, expected: Expected) -> anyhow::Result<()> {
    // The server's own reading, whole: the file, the `${MEDIACORE_*}` in it and
    // `Config::validate`. It carries its own refusals, which name the path and
    // the setting, so nothing here wraps them in a second sentence.
    let config = Config::load(path)?;

    let mut met = Vec::new();
    let mut refused = Vec::new();
    // Asked whatever the deployment stated, because it is not a statement of
    // intent: it is the one thing the server does with this config on the way up
    // that `Config::validate` does not, and the line printed below claims this
    // server accepts the file.
    for answer in std::iter::once(listen(&config)).chain(answers(&config, expected)) {
        match answer {
            Ok(line) => met.push(line),
            Err(why) => refused.push(why),
        }
    }

    anyhow::ensure!(refused.is_empty(), "{}", refusal(path, &refused));

    println!("check: {path} is complete, and this server accepts it");
    for line in met {
        println!("check: {line}");
    }

    Ok(())
}

/// What every expectation this deployment stated has to say about this config.
///
/// Split from [`run`] so that the flags can be tested for being independent of
/// one another, which is the property a chart relies on: a deployment with no
/// database and no bucket that asked for neither has to pass.
fn answers(config: &Config, expected: Expected) -> Vec<Answer> {
    let mut answers = Vec::new();

    if expected.postgres {
        answers.push(postgres(config));
    }
    if expected.recording {
        answers.push(recording(config));
    }
    if expected.routable_advertise {
        answers.push(advertise(&config.rtc));
    }

    answers
}

/// The address the server binds the signalling port to, built the way `main`
/// builds it.
///
/// **The one refusal here that no flag asks for**, because it is not a
/// statement of intent -- there is no deployment that meant `bind_addresses:
/// [eth0]`. `Config::validate` counts the bind addresses and never reads one,
/// so a name or an interface where an address belongs is a config it accepts
/// and a process that exits on `invalid socket address syntax` with nothing
/// named -- the restart loop this subcommand exists to replace, over a typo.
///
/// Built with the same `format!` as `main` rather than from the parts, for the
/// module's rule about reading the config the way the server reads it: an
/// address joined to a port with a colon is why an ipv6 bind address has to
/// carry its brackets, and a check that knew better than the server would pass
/// a `::` the server then dies on.
fn listen(config: &Config) -> Answer {
    // `Config::validate` has already refused an empty list, and it runs inside
    // the `Config::load` above.
    let address = format!("{}:{}", config.bind_addresses[0], config.port);

    match address.parse::<SocketAddr>() {
        Ok(address) => Ok(format!("signalling will listen on {address}")),
        Err(error) => Err(format!(
            "bind_addresses[0] and port make `{address}`, which is not an address this server can \
             listen on ({error}). Nothing else would have caught it: the server's own validation \
             makes sure there is a bind address and never reads what is in it, so this config \
             starts a process that builds this string, fails on it, and exits with that sentence \
             and no setting named. bind_addresses takes ip addresses -- an interface name or a \
             host name is not one -- and an ipv6 address is written in brackets, `[::]`, because \
             the port is joined to it with a colon"
        )),
    }
}

/// One error carrying everything that was refused.
///
/// The opening sentence is the whole reason this subcommand exists, so it has to
/// stay true of every refusal under it -- including [`listen`]'s, which is a
/// config the server's validation accepts and then dies on rather than one it
/// runs and runs wrongly.
fn refusal(path: &str, refused: &[String]) -> String {
    let mut text = format!(
        "{path} is a config this server's own validation accepts, and it is still not the one to \
         start this deployment with:"
    );

    for why in refused {
        text.push_str("\n  - ");
        text.push_str(why);
    }

    text
}

/// The deployment keeps its meetings in a database.
fn postgres(config: &Config) -> Answer {
    if config.sessions.in_postgres() {
        // Never the url, and never anything taken out of it. It carries a
        // password in every deployment that has one, and this line goes to the
        // log of a container that anybody with `kubectl logs` can read.
        return Ok(
            "sessions.postgres.url names a database, so a meeting record is a row".to_owned(),
        );
    }

    Err(format!(
        "sessions.postgres.url is empty and this deployment asked for a database \
         (--expect-postgres). An empty url is not a mistake the server can see: it is how a \
         config says \"no database\", so it would start, answer everything, and write every \
         meeting record into {} instead -- a directory one process owns, on a volume only if \
         somebody mounted one there. Nothing would be recorded either, because an upload is a \
         row several nodes claim and a directory cannot be a queue. Set MEDIACORE_POSTGRES_URL \
         in the environment of the container that reads this config, or stop passing \
         --expect-postgres if a directory really was what was meant",
        config.sessions.directory.display()
    ))
}

/// The deployment records meetings, so there has to be somewhere to put them.
fn recording(config: &Config) -> Answer {
    let s3 = &config.recording.s3;
    // Named one by one rather than as "the object store is not configured": a
    // Secret is very often three keys of four, and the one that is missing is
    // the whole of what has to be read off this line and fixed.
    let missing: Vec<&str> = [
        ("recording.s3.endpoint", &s3.endpoint),
        ("recording.s3.bucket", &s3.bucket),
        ("recording.s3.access_key_id", &s3.access_key_id),
        ("recording.s3.secret_access_key", &s3.secret_access_key),
    ]
    .into_iter()
    .filter(|(_, value)| value.trim().is_empty())
    .map(|(name, _)| name)
    .collect();

    if missing.is_empty() {
        // The bucket and the endpoint are in the ConfigMap this check just
        // read; the two keys beside them are not, and are not printed.
        return Ok(format!(
            "recording has somewhere to go: the bucket `{}` at {}",
            s3.bucket, s3.endpoint
        ));
    }

    Err(format!(
        "this deployment asked for recording (--expect-recording) and {} empty: {}. A config \
         with no bucket named and recording.enabled off is exactly how a deployment that records \
         nothing is written, so the server cannot tell this one from that: it would start, build \
         no recorder at all, and a room that asks in its own metadata to be recorded would not \
         be -- nothing refused, nothing logged, and the meeting discovered to be missing by \
         whoever asks for it afterwards. The values arrive from the environment as MEDIACORE_S3_*, \
         in the container that reads this config",
        if missing.len() == 1 {
            "this setting is"
        } else {
            "these settings are"
        },
        missing.join(", ")
    ))
}

/// The deployment takes media from clients that are not on this node.
fn advertise(rtc: &RtcConfig) -> Answer {
    let stated = advertised(rtc);

    if stated.is_empty() {
        return Err(
            "nothing in this config states the media address -- rtc.advertise_addresses names \
             nothing, rtc.node_ip is empty, and the media socket's own address is not a statement \
             either (rtc.bind_address is the wildcard, or rtc.use_socket_address is off) -- and \
             this deployment asked for one clients off this node can reach \
             (--routable-advertise). With none of them the server picks an address for itself, \
             the one a route to the outside leaves from, and inside a pod that is the pod's \
             address on the cluster network, which nothing outside the node routes to. Set \
             MEDIACORE_ADVERTISE_IP to the node's own address; under Kubernetes that is the \
             downward API's status.hostIP"
                .to_owned(),
        );
    }

    // Every entry that cannot work rather than the first of them, for the reason
    // this module opens with: a list that is wrong is usually wrong more than
    // once -- `status.hostIP` that never arrived beside a second address copied
    // out of the sample -- and one name per restart is one outage per name.
    let refused: Vec<String> = stated
        .iter()
        .filter_map(|ip| unroutable(*ip).map(|because| format!("`{ip}`, which is {because}")))
        .collect();

    if !refused.is_empty() {
        let (addresses, goes) = match refused.as_slice() {
            [only] => (only.clone(), "The address goes"),
            several => (several.join("; and "), "Each of those goes"),
        };

        return Err(format!(
            "this deployment asked for a media address clients off this node can reach \
             (--routable-advertise), and it advertises {addresses}. {goes} into every ice \
             candidate verbatim, so the server would answer every request, hand that out, and \
             receive no media: a call that connects, lists everybody, and is silent. Set \
             MEDIACORE_ADVERTISE_IP to the address a client outside this host routes to -- under \
             Kubernetes the node's own, from the downward API's status.hostIP. A private address \
             is not refused here: behind a relay or a port forward it is the ordinary case and is \
             what production runs"
        ));
    }

    Ok(format!(
        "media is advertised as {}: not the wildcard, not loopback, not link-local",
        stated
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The addresses the media engine will really hand out, or nothing if it would
/// have to guess one.
///
/// Read in the order `resolve_advertise` in `crates/server/src/media.rs` reads
/// it, and stopping exactly where that stops being a statement somebody made.
/// `RtcConfig::advertise` is the call `MediaEngine::start` itself makes, so what
/// is judged is the list a client is handed -- entries written as `ip:port`
/// included -- and not a second reading of the same strings; the port it asks
/// for has nothing to do with whether an address can be reached, so zero stands
/// in for it.
///
/// The step after the three below is `primary_local_ip`, the address a route to
/// the outside leaves from. That one is the guess this flag exists to refuse,
/// so the chain is followed no further and an empty answer means "the server
/// would pick one for itself".
fn advertised(rtc: &RtcConfig) -> Vec<IpAddr> {
    let stated: Vec<IpAddr> = rtc.advertise(0).iter().map(SocketAddr::ip).collect();
    if !stated.is_empty() {
        return stated;
    }

    if let Ok(ip) = rtc.node_ip.parse::<IpAddr>() {
        return vec![ip];
    }

    // A config that binds the media socket to one interface instead of the
    // wildcard has named an address as surely as `advertise_addresses` would
    // have, and with `use_socket_address` on -- which is the default -- that is
    // the address the engine advertises. Refusing it as "nothing was stated"
    // would refuse a deployment that works, which is the one thing a check like
    // this must never do.
    if rtc.use_socket_address
        && let Ok(ip) = rtc.bind_address.parse::<IpAddr>()
        && !ip.is_unspecified()
    {
        return vec![ip];
    }

    Vec::new()
}

/// Why no client off this node could send media to an address, or `None` if
/// nothing here says it could not.
///
/// **Private ranges are deliberately absent.** A cluster whose nodes sit on
/// 10.0.0.0/8 behind a relay or a port forward is the normal deployment and is
/// what production runs, so refusing one would refuse the only shape most of
/// these ever take. What is left is the set of addresses that cannot work for
/// anybody, ever: they are wrong on a LAN exactly as they are wrong on the
/// internet.
///
/// **Why link-local is in the list.** It is not a value anybody types. It is
/// what a machine gives itself when it asks for a lease and nothing answers, so
/// a node whose DHCP failed reports one as `status.hostIP` and the chart fills
/// it in -- and the deployment then fails in precisely the way loopback does,
/// silently and only for the people who are not on that wire.
fn unroutable(ip: IpAddr) -> Option<&'static str> {
    match unmapped(ip) {
        ip if ip.is_unspecified() => Some(
            "the wildcard -- an instruction to a socket about what to listen on, and not an \
             address anything can send to",
        ),
        ip if ip.is_loopback() => Some(
            "the loopback address -- this container and nothing else, so a client anywhere else \
             would send its media into its own machine",
        ),
        ip if link_local(ip) => Some(
            "a link-local address (169.254.0.0/16, fe80::/10) -- what a machine gives itself when \
             it asks for a lease and nothing answers, so most likely a node whose DHCP failed, \
             and reachable only from the wire it is plugged into",
        ),
        _ => None,
    }
}

/// `::ffff:10.0.0.5` and `10.0.0.5` are one address written two ways.
///
/// Judged as it stands, the first spelling is neither loopback nor link-local
/// nor the wildcard whatever it holds, so `::ffff:127.0.0.1` would pass every
/// test below it. Both spellings are read as the v4 one instead.
fn unmapped(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// 169.254.0.0/16 and fe80::/10.
///
/// The v6 half is written out rather than asked of the standard library, whose
/// `is_unicast_link_local` is still unstable on the pinned toolchain. The mask
/// is the prefix RFC 4291 section 2.5.6 gives link-local unicast.
fn link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    }
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;

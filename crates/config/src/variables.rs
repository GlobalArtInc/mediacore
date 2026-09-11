//! Filling `${MEDIACORE_...}` in from the environment, before the text is read
//! as yaml.
//!
//! **Why the server does this rather than something in front of it.** A secret
//! used to reach the config by being written into a file: `deploy/start-sfu.sh`
//! ran `envsubst` over `deploy/mediacore.yaml.template`, and a chart's init
//! container ran the same thing into an `emptyDir`. The rendered copy -- api
//! secret, database password, object store credentials, in cleartext -- then
//! sat on a filesystem for the life of the container, readable by every process
//! in it and by anything that later mounted the same volume. Doing it here
//! means the file the deployment wrote is the file the server reads: a
//! ConfigMap keeps the literal `${MEDIACORE_API_SECRET}`, the secret stays an
//! environment variable of one process, and there is no rendered copy to leak,
//! to back up, or to leave behind on a volume.
//!
//! **An unset name is a refusal, and an empty value is not.** `envsubst`
//! substitutes nothing and says nothing, so `${MEDIACORE_API_SECRET}` in a
//! deployment that forgot to set it becomes an empty string -- a server that
//! starts, answers, and rejects every token anybody presents to it, which is
//! exactly the class of failure this project refuses to ship. A name that is
//! *set* to an empty value is a different statement and is honoured: that is
//! how `MEDIACORE_POSTGRES_URL=` says "no database, keep meetings in the
//! directory". Unset says nothing at all, and nothing is not an answer.
//!
//! **What may be filled in: a name beginning with `MEDIACORE_`, and nothing
//! else.** `envsubst` was handed an explicit list of seven names, and that list
//! was a real safety property rather than paperwork -- a `${...}` anywhere else
//! in the file, in a room's metadata or a password or a comment, was left
//! exactly as written. A prefix keeps that property with one place to maintain
//! instead of three: today the list of names lives in the init container, again
//! in `start-sfu.sh`, and a third time in a test whose whole job is to catch
//! those two drifting apart. With a prefix the names live in the config file
//! that uses them, and a deployment that adds a variable adds nothing
//! anywhere else. It is also the namespace this deployment already lives in --
//! every variable in `deploy/.env.example` is `MEDIACORE_*`, as are all seven
//! of the chart's -- and it is far narrower than the obvious alternative of
//! "any name the environment happens to have", which would make `${HOME}` in a
//! comment an expansion and a password containing `${...}` a startup refusal.
//! An allowlist inside the config was the other candidate and is a chicken and
//! an egg: it would have to be read out of the very file it governs, and it is
//! one more place to forget a name.
//!
//! **A near miss is refused, not left as text.** `${mediacore_api_secret}`,
//! `${MEDIACORE_API_SECRET ` with a space before the brace and the bare
//! `$MEDIACORE_API_SECRET` all end the same way: an api secret that is
//! literally that string, on a server that starts and rejects every token. The
//! bare form is the sharpest of the three, because `envsubst` *did* fill it in,
//! so a file that worked before this change would quietly stop working after
//! it. All three are refused by name, and all three have `$$` to fall back on.
//!
//! **The substitution is textual and a value is never rescanned.** Textual,
//! because one of the things filled in is a whole `ice_servers:` block that
//! `start-sfu.sh` builds -- a template can fill a value in and cannot leave a
//! block out -- so this cannot work on the parsed document. The cost is the one
//! `envsubst` had: a value carrying a `"` or a newline can change the shape of
//! the yaml around it, which is why `start-sfu.sh` refuses a TURN secret with a
//! quote in it. Never rescanned, because a value is data: a database password
//! that happens to contain `${MEDIACORE_API_SECRET}` or `$$` comes through as
//! itself and cannot reach for anything.

/// Every name this server will fill in begins with this.
pub const PREFIX: &str = "MEDIACORE_";

/// Fill in `${MEDIACORE_...}` from this process's environment.
pub fn expand(body: &str) -> anyhow::Result<String> {
    // `ok()` reads a value that is not utf-8 as no value at all. A name whose
    // bytes this server cannot read is a name it cannot fill in, and "set it
    // where the server runs" is the right instruction either way.
    expand_with(body, |name| std::env::var(name).ok())
}

/// The same, against any source of values.
///
/// The tests want this: a process cannot set an environment variable for one of
/// its own threads, and the tests of this module run beside each other in one
/// binary. It is also how [`crate::Config::parse`] stays a function about text
/// and nothing else.
pub fn expand_with(body: &str, lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let mut rendered = String::with_capacity(body.len());
    // Collected rather than raised at the first one. A deployment that forgot
    // its database url has very likely forgotten the bucket credentials beside
    // it, and being told one name per restart is one outage per name.
    let mut missing: Vec<&str> = Vec::new();
    let mut rest = body;

    while let Some(dollar) = rest.find('$') {
        rendered.push_str(&rest[..dollar]);
        let tail = &rest[dollar + 1..];

        // `$$` is one dollar, and it is the way out of every refusal below. A
        // password, a piece of room metadata or a shell snippet in a comment may
        // legitimately contain a dollar, and with no escape there would be no
        // way to write one.
        if let Some(after) = tail.strip_prefix('$') {
            rendered.push('$');
            rest = after;
            continue;
        }

        let Some(inside) = tail.strip_prefix('{') else {
            let name = leading_name(tail);
            anyhow::ensure!(
                !ours(name),
                "the config has `${name}` with no braces around the name. This server fills in \
                 `${{{name}}}` and only that shape, so what is written would stay in the file as \
                 text, and an api secret that is literally `$MEDIACORE_API_SECRET` is a server \
                 that starts and rejects every token. `envsubst`, which used to render this file, \
                 did fill the bare form in -- so this is refused rather than passed through: put \
                 braces around the name, or write `$$` for a literal dollar"
            );

            // A dollar in front of anything else is a dollar, including one at
            // the very end of the file: `tail` is then empty and the loop ends
            // on the next pass.
            rendered.push('$');
            rest = tail;
            continue;
        };

        let name = leading_name(inside);
        let Some(after) = inside[name.len()..].strip_prefix('}') else {
            anyhow::ensure!(
                !ours(name),
                "the config opens a `${{` in front of `{name}` and never closes it with a `}}`. A \
                 name is letters, digits and underscores between the braces, so a stray space or \
                 a dash is enough to leave a secret in the file as text: write `${{{name}}}` \
                 exactly, or `$$` if a literal dollar was meant"
            );

            rendered.push_str("${");
            rest = inside;
            continue;
        };

        if name.starts_with(PREFIX) {
            match lookup(name) {
                Some(value) => rendered.push_str(&value),
                // Nothing is written for it, and nothing is returned either:
                // the refusal is raised below once every name is known.
                None if !missing.contains(&name) => missing.push(name),
                None => {}
            }
        } else {
            anyhow::ensure!(
                !ours(name),
                "the config asks for `{name}`, and this server fills in names beginning with \
                 `{PREFIX}`. Environment variable names are case sensitive, so `{name}` is not \
                 that name and would be left in the file as text: write it in upper case, or take \
                 the braces off if it was meant to stay as it is"
            );

            // Left exactly as written, which is the whole point of the prefix:
            // a `${...}` this server does not own is somebody else's text.
            rendered.push('$');
            rendered.push('{');
            rendered.push_str(name);
            rendered.push('}');
        }

        rest = after;
    }

    rendered.push_str(rest);

    anyhow::ensure!(
        missing.is_empty(),
        "the config asks for {} the environment does not have: {}. Set {} where this server runs \
         -- deploy/.env for the compose stack, the container's own environment under Kubernetes -- \
         and note that empty is a value: `MEDIACORE_POSTGRES_URL=` is how a deployment says \"no \
         database\", while leaving it unset says nothing at all. Unset is refused rather than \
         filled in with an empty string because a server whose api secret expanded to nothing \
         starts and then rejects every token. If a literal dollar was meant, write it as `$$`",
        if missing.len() == 1 {
            "a variable"
        } else {
            "variables"
        },
        missing.join(", "),
        if missing.len() == 1 { "it" } else { "them" }
    );

    Ok(rendered)
}

/// The run of characters an environment variable name is made of.
///
/// All ascii, so a byte index is a character boundary and the slice cannot
/// split anything.
fn leading_name(text: &str) -> &str {
    let end = text
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(text.len());
    &text[..end]
}

/// Whether a name is close enough to one of ours that leaving it as text would
/// be a silent failure rather than a deliberate piece of writing.
///
/// Case insensitively, so `${mediacore_api_secret}` is refused with the reason
/// instead of becoming an api secret spelled exactly like that. `get` rather
/// than a slice, because a name shorter than the prefix is the common case and
/// must not panic.
fn ours(name: &str) -> bool {
    name.get(..PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(PREFIX))
}

#[cfg(test)]
#[path = "variables_tests.rs"]
mod tests;

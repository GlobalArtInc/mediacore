//! Who else is running this, and what each of them is holding.
//!
//! One row per process in the shared store, rewritten by that process on its
//! own clock, and one read of the whole table kept in memory here. Nothing on
//! the join path ever touches the database for it: steering is a lookup in a
//! snapshot that a background task replaces, so a database that is briefly away
//! costs the cluster its freshness and costs a call nothing at all.
//!
//! **Off unless asked.** With `cluster.advertise_url` empty this module starts
//! nothing, publishes nothing and answers that this node is alone -- which is
//! what every deployment running one replica gets, unchanged.

use std::sync::{Arc, Mutex};

use mediacore_config::Config;
use mediacore_room::Registry;
use mediacore_session::{Node, NodeState, SessionStore, StoreError};
use tokio::task::JoinHandle;



/// What this node knew about the cluster when it last looked.
///
/// Replaced whole rather than edited, so a reader holding one is holding a
/// consistent picture of one moment and never half of two.
#[derive(Debug, Default)]
pub struct View {
    pub nodes: Vec<Node>,
    pub read_at_ms: i64,
    lease_ms: i64,
}

impl View {
    /// How long a node may go quiet before this view stops counting it. Read
    /// by whoever renders the view, so that `alive` is worked out in one place.
    pub fn lease_ms(&self) -> i64 {
        self.lease_ms
    }
}

impl View {
    /// Which node is holding this room, among those still beating.
    ///
    /// A room nobody publishes has no owner, which is the ordinary answer for
    /// a room that does not exist yet. A room two nodes both publish is
    /// answered with the one whose beat is older: they have to agree, and the
    /// earlier claim is the one the other should let go of.
    pub fn owner_of(&self, room: &str) -> Option<&Node> {
        self.nodes
            .iter()
            .filter(|node| node.alive(self.read_at_ms, self.lease_ms))
            .filter(|node| node.rooms.iter().any(|held| held == room))
            .min_by_key(|node| (node.started_at_ms, node.id.clone()))
    }

    /// The nodes that should be given something new.
    pub fn taking_work(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .iter()
            .filter(|node| node.takes_work(self.read_at_ms, self.lease_ms))
    }

    /// Which node should hold this room, whether or not anybody holds it yet.
    ///
    /// A room somebody publishes belongs to them: what is happening beats what
    /// ought to be. A room nobody holds is scored, and the highest score wins
    /// -- every node computing the same answer from the same table without
    /// asking anybody, which is what stops two nodes both opening a meeting for
    /// the same name in the window before either has published it.
    ///
    /// A draining node is not offered an unheld room, and keeps the ones it
    /// has: people are still in those meetings.
    pub fn should_hold(&self, room: &str) -> Option<&Node> {
        if let Some(owner) = self.owner_of(room) {
            return Some(owner);
        }

        self.taking_work().max_by_key(|node| score(room, &node.id))
    }

    /// Every node still beating, draining ones included.
    pub fn alive(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .iter()
            .filter(|node| node.alive(self.read_at_ms, self.lease_ms))
    }
}

/// This node's own row, and the task that keeps writing it.
pub struct Cluster {
    id: String,
    /// Minted here and never reused. It is what proves a row is ours: a beat
    /// that finds a different boot in our own row is another process wearing
    /// our name, which is a configuration mistake no lease will resolve.
    boot: String,
    joined: bool,
    /// Raised when this node begins letting go. The beat reads it, so the rest
    /// of the cluster is told to stop giving this node work on the next tick
    /// rather than only finding out when it stops beating altogether.
    draining: Arc<std::sync::atomic::AtomicBool>,
    sessions: Arc<SessionStore>,
    view: Arc<Mutex<Arc<View>>>,
    beat: Option<JoinHandle<()>>,
}

impl Cluster {
    /// Starts beating, or does not, depending on one setting.
    pub fn start(config: &Config, sessions: Arc<SessionStore>, registry: Arc<Registry>) -> Self {
        let id = config.node_id.clone();
        let boot = uuid::Uuid::new_v4().simple().to_string();
        let view = Arc::new(Mutex::new(Arc::new(View {
            lease_ms: config.cluster.lease_ms(),
            ..View::default()
        })));

        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));

        if !config.cluster.joined() {
            return Self {
                id,
                boot,
                joined: false,
                draining,
                sessions,
                view,
                beat: None,
            };
        }

        let row = Row {
            id: id.clone(),
            boot: boot.clone(),
            signal_url: config.cluster.advertise_url.trim().to_owned(),
            advertise: config.rtc.advertise_addresses.join(","),
            version: mediacore_proto::SERVER_VERSION.to_owned(),
            region: config.region.clone(),
            started_at_ms: now_ms(),
        };

        tracing::info!(
            node = %row.id,
            signal_url = %row.signal_url,
            lease_seconds = config.cluster.lease_seconds,
            "this node is one of several and will say so"
        );

        let beat = tokio::spawn(beat(
            row,
            config.cluster.beat(),
            config.cluster.lease_ms(),
            Arc::clone(&sessions),
            registry,
            Arc::clone(&view),
            Arc::clone(&draining),
        ));

        Self {
            id,
            boot,
            joined: true,
            draining,
            sessions,
            view,
            beat: Some(beat),
        }
    }

    /// Says this node is on its way out, on the next beat.
    pub fn begin_draining(&self) {
        self.draining
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Takes this node's row away.
    ///
    /// A planned stop converges in a beat rather than in a lease: without this
    /// the others would go on offering work to a process that has already
    /// exited until its thirty seconds ran out. Blocking, because it is the
    /// last thing this node does and there is nothing left to be asynchronous
    /// for; a failure is logged and nothing more, since the lease is the
    /// backstop that makes this an optimisation rather than a requirement.
    pub fn leave(&self) {
        if !self.joined {
            return;
        }

        match self.sessions.forget_node(&self.id) {
            Ok(()) => tracing::info!(node = %self.id, "this node is no longer in the cluster"),
            Err(error) => tracing::warn!(
                node = %self.id,
                %error,
                "this node could not take its own row away; the others will notice when its lease \
                 runs out"
            ),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn boot(&self) -> &str {
        &self.boot
    }

    /// Whether this node is registered with any others.
    pub fn joined(&self) -> bool {
        self.joined
    }

    /// The last read of the table. Cheap: one lock and one `Arc` clone.
    pub fn view(&self) -> Arc<View> {
        Arc::clone(&self.view.lock().expect("cluster view lock"))
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        if let Some(beat) = self.beat.take() {
            beat.abort();
        }
    }
}

/// The parts of a row that do not change between beats.
#[derive(Clone)]
struct Row {
    id: String,
    boot: String,
    signal_url: String,
    advertise: String,
    version: String,
    region: String,
    started_at_ms: i64,
}

async fn beat(
    row: Row,
    every: std::time::Duration,
    lease_ms: i64,
    sessions: Arc<SessionStore>,
    registry: Arc<Registry>,
    view: Arc<Mutex<Arc<View>>>,
    draining: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticks.tick().await;

        let now = now_ms();
        let node = Node {
            id: row.id.clone(),
            boot: row.boot.clone(),
            signal_url: row.signal_url.clone(),
            advertise: row.advertise.clone(),
            version: row.version.clone(),
            region: row.region.clone(),
            state: match draining.load(std::sync::atomic::Ordering::Acquire) {
                true => NodeState::Draining,
                false => NodeState::Live,
            },
            started_at_ms: row.started_at_ms,
            kept_alive_at_ms: now,
            rooms: registry.rooms().into_iter().map(|room| room.name.clone()).collect(),
        };

        // Publishing and reading are one round of the same task rather than two
        // timers, so what this node reads is never older than what it wrote.
        let sessions = Arc::clone(&sessions);
        let read = tokio::task::spawn_blocking(move || {
            let published = sessions.publish_node(&node);
            let seen = sessions.nodes();
            (published, seen)
        })
        .await;

        let Ok((published, seen)) = read else {
            continue;
        };

        match published {
            Ok(()) => {}
            // Never transient: two processes are configured with one id, and
            // they are already taking each other's uploads and writing each
            // other's meetings. Said loudly and repeatedly, because the thing
            // that fixes it is a person changing a configuration.
            Err(StoreError::Taken(what)) => tracing::error!(
                node = %row.id,
                "{what}"
            ),
            Err(error) => tracing::warn!(node = %row.id, %error, "this node could not say it is up"),
        }

        if let Ok(nodes) = seen {
            *view.lock().expect("cluster view lock") = Arc::new(View {
                nodes,
                read_at_ms: now,
                lease_ms,
            });
        }
    }
}

/// How well one node suits one room.
///
/// Rendezvous hashing: the score depends on the pair and on nothing else, so
/// every node ranks the same candidates the same way, and a node arriving or
/// leaving moves only the rooms that were scored highest for it rather than
/// reshuffling the lot.
///
/// Sha-256 rather than the standard hasher, whose output is explicitly not
/// stable between Rust versions -- two nodes on two builds scoring differently
/// is exactly the split this exists to prevent.
fn score(room: &str, node: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(room.as_bytes());
    hasher.update([0]);
    hasher.update(node.as_bytes());
    hasher.finalize().into()
}

/// This node's clock, in the same units every record here is kept in.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "cluster_tests.rs"]
mod tests;

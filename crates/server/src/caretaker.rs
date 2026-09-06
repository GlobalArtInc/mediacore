use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mediacore_room::Registry;
use tokio::task::JoinHandle;

/// The clock the registry does not have.
///
/// Everything else that happens to a room happens because somebody did
/// something to it: a participant joined or left, or the room service was
/// called. Two things have to happen when nobody does anything at all, and
/// neither of them can hang off a participant's session:
///
/// * A meeting's grace period runs out. Every meeting has one, whether or not
///   the deployment configured a session policy: a room falls below the number
///   of people it needs -- one, unless a policy asks for more -- and the
///   meeting ends when nobody has come back. The room it happened in is
///   usually already gone by then, thrown away with the last person to leave,
///   so there is nothing else left that could notice.
/// * A session policy runs out. The grace is armed by the departure that drops
///   the room below its floor, and the room can empty completely before it
///   expires. Driven from a signal session, the sweep then has nobody to run
///   it: the deadline sits in the past until somebody opens the room the next
///   morning, and the first tick of *their* session ends the meeting and
///   disconnects them with `ROOM_CLOSED`. The person the policy fires on is
///   the one person it was never about.
/// * An empty room is collected. A room somebody created outlives the last
///   person to leave it, which is the point of creating one -- but a service
///   that makes a room per meeting and never takes one away grows its registry
///   and its `ListRooms` for as long as the process lives. `empty_timeout` and
///   `departure_timeout` are the numbers LiveKit already carries for this, and
///   until there was a clock they were reported and never enforced.
///
/// So the driver is one task per server, holding nothing but the registry, and
/// it is owned by the state it sweeps: the last clone of `AppState` to go takes
/// the task with it rather than leaving it ticking over a registry nobody else
/// can reach.
pub struct Caretaker {
    task: JoinHandle<()>,
}

impl Caretaker {
    /// Starts the sweep. Must be called from inside a Tokio runtime.
    pub fn start(registry: Arc<Registry>, interval: Duration) -> Self {
        let task = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticks.tick().await;

            loop {
                ticks.tick().await;

                let swept = registry.sweep(now_ms());
                if !swept.is_empty() {
                    tracing::debug!(
                        ended = ?swept.ended,
                        closed = ?swept.closed,
                        sessions = ?swept.sessions,
                        "the sweep ended meetings and took rooms away"
                    );
                }
            }
        });

        Self { task }
    }
}

impl Drop for Caretaker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as i64
}

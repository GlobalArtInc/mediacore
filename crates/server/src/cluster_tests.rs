use super::*;

fn node(id: &str, started: i64, beat: i64, state: NodeState, rooms: &[&str]) -> Node {
    Node {
        id: id.to_owned(),
        boot: format!("boot-{id}"),
        signal_url: format!("ws://{id}:7880"),
        advertise: String::new(),
        version: "test".to_owned(),
        region: String::new(),
        state,
        started_at_ms: started,
        kept_alive_at_ms: beat,
        rooms: rooms.iter().map(|room| (*room).to_owned()).collect(),
    }
}

fn view(nodes: Vec<Node>, now: i64) -> View {
    View {
        nodes,
        read_at_ms: now,
        lease_ms: 30_000,
    }
}

#[test]
fn the_node_publishing_a_room_is_the_one_that_holds_it() {
    let seen = view(
        vec![
            node("ND_a", 1_000, 100_000, NodeState::Live, &["standup"]),
            node("ND_b", 1_000, 100_000, NodeState::Live, &["retro"]),
        ],
        100_000,
    );

    assert_eq!(seen.owner_of("standup").unwrap().id, "ND_a");
    assert_eq!(seen.owner_of("retro").unwrap().id, "ND_b");
    assert!(
        seen.owner_of("nobody-made-this").is_none(),
        "a room nobody holds was given an owner"
    );
}

/// A node that stopped beating cannot still be holding anything: its rooms are
/// gone with it, and steering to it would send somebody to a socket that is
/// not there.
#[test]
fn a_node_that_stopped_beating_holds_nothing() {
    let seen = view(
        vec![node("ND_gone", 1_000, 10_000, NodeState::Live, &["standup"])],
        10_000 + 30_001,
    );

    assert!(seen.owner_of("standup").is_none());
    assert_eq!(seen.alive().count(), 0);
}

/// Two nodes both publishing one room is the split this exists to survive.
/// They have to agree on which of them it is, from the row alone and without
/// asking each other, so the answer is the older node -- the same answer on
/// every reader.
#[test]
fn two_nodes_holding_one_room_agree_on_which_of_them_it_is() {
    let older = node("ND_b", 1_000, 100_000, NodeState::Live, &["standup"]);
    let newer = node("ND_a", 5_000, 100_000, NodeState::Live, &["standup"]);

    let one = view(vec![older.clone(), newer.clone()], 100_000);
    let other = view(vec![newer, older], 100_000);

    assert_eq!(one.owner_of("standup").unwrap().id, "ND_b");
    assert_eq!(
        other.owner_of("standup").unwrap().id,
        "ND_b",
        "two readers of the same table disagreed about who owns a room"
    );
}

/// Draining is not death. The node still answers for what it holds -- somebody
/// is still in those meetings -- and is simply not given anything new.
#[test]
fn a_draining_node_still_owns_its_rooms_and_is_offered_nothing() {
    let seen = view(
        vec![
            node("ND_going", 1_000, 100_000, NodeState::Draining, &["standup"]),
            node("ND_staying", 2_000, 100_000, NodeState::Live, &[]),
        ],
        100_000,
    );

    assert_eq!(
        seen.owner_of("standup").unwrap().id,
        "ND_going",
        "a meeting was taken away from the node still serving it"
    );
    assert_eq!(seen.alive().count(), 2);
    assert_eq!(
        seen.taking_work().map(|node| node.id.as_str()).collect::<Vec<&str>>(),
        vec!["ND_staying"]
    );
}

/// A node on its own answers every question about itself and never looks at a
/// table, which is what every deployment running one replica gets.
#[test]
fn a_node_that_never_joined_holds_an_empty_view() {
    let alone = view(Vec::new(), 100_000);

    assert!(alone.owner_of("standup").is_none());
    assert_eq!(alone.alive().count(), 0);
}

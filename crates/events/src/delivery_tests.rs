
use super::*;

#[test]
fn the_wait_doubles_and_then_stops_doubling() {
    let retry = Retry {
        attempts: 8,
        backoff: Duration::from_millis(500),
        max_backoff: Duration::from_secs(4),
    };

    assert_eq!(retry.wait(1), Duration::from_millis(500));
    assert_eq!(retry.wait(2), Duration::from_secs(1));
    assert_eq!(retry.wait(3), Duration::from_secs(2));
    assert_eq!(retry.wait(4), Duration::from_secs(4));
    assert_eq!(
        retry.wait(8),
        Duration::from_secs(4),
        "an endpoint that came back a moment ago is waiting out a backoff measured in \
             minutes, because nothing capped the doubling"
    );
}

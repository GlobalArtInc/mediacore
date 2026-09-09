
use super::*;

fn limiter(rate: u32, burst: u32) -> DataLimiter {
    DataLimiter::new(&DataConfig {
        enabled: true,
        max_packet_bytes: 64_000,
        rate,
        burst,
    })
}

#[test]
fn a_burst_is_admitted_and_the_next_packet_is_not() {
    let mut limiter = limiter(10, 20);
    let now = Instant::now();

    for _ in 0..20 {
        assert!(limiter.admit(now));
    }

    assert!(!limiter.admit(now));
}

#[test]
fn the_bucket_refills_at_the_rate_it_was_given() {
    let mut limiter = limiter(10, 20);
    let now = Instant::now();

    for _ in 0..20 {
        assert!(limiter.admit(now));
    }

    assert!(!limiter.admit(now));
    assert!(limiter.admit(now + Duration::from_millis(100)));
    assert!(!limiter.admit(now + Duration::from_millis(100)));
}

#[test]
fn a_burst_under_the_rate_is_lifted_to_it() {
    let mut limiter = limiter(10, 1);
    let now = Instant::now();

    for _ in 0..10 {
        assert!(limiter.admit(now));
    }

    assert!(!limiter.admit(now));
}

#[test]
fn a_router_with_nobody_in_it_delivers_nothing() {
    let router = DataRouter::default();

    assert!(!router.send("PA_nobody", DataChannel::Reliable, b"anything"));
    assert_eq!(
        router.totals(),
        DataTotals {
            peers: 0,
            packets: 0,
            bytes: 0,
            dropped: 0,
        }
    );
}

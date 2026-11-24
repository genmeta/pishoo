use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use super::{OCSP_PERIOD, ocsp_ticks, wake_due_ocsp};

#[tokio::test(start_paused = true)]
async fn ocsp_refresh_runs_after_sleep_without_replaying_missed_checks() {
    let mut ticks = ocsp_ticks();
    let started = Instant::now();
    let wall_start = SystemTime::UNIX_EPOCH;
    let deadline = wall_start + OCSP_PERIOD;

    assert!(!wake_due_ocsp(&mut ticks, wall_start, deadline));

    // Simulate several days of sleep without advancing the monotonic timer.
    let woke = wall_start + OCSP_PERIOD * 3;
    assert!(wake_due_ocsp(&mut ticks, woke, deadline));
    assert_eq!(ticks.tick().await, started);
    assert!(!wake_due_ocsp(&mut ticks, woke, woke + OCSP_PERIOD));
    assert!(
        tokio::time::timeout(OCSP_PERIOD - Duration::from_secs(1), ticks.tick())
            .await
            .is_err()
    );
    assert_eq!(ticks.tick().await, started + OCSP_PERIOD);
}

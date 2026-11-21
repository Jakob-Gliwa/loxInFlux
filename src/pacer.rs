//! Minimum spacing between grabber commands.
//!
//! Miniserver firmware 17.3 closes the WebSocket with code 4020 and then blocks
//! the client's IP when commands arrive in a burst. Every grabber command
//! reserves the next free slot before it is sent, so at most `per_second`
//! commands start in any second, however many requests are in flight. Unused
//! slots do not accumulate: after an idle period the next command starts
//! immediately, the one after it a full period later.

use std::{sync::Mutex, time::Duration};
use tokio::time::{Instant, sleep_until};

pub struct Pacer {
    period: Option<Duration>,
    next: Mutex<Instant>,
}

impl Pacer {
    /// `per_second == 0` disables pacing.
    pub fn new(per_second: u32) -> Self {
        Self {
            period: (per_second > 0).then(|| Duration::from_secs(1) / per_second),
            next: Mutex::new(Instant::now()),
        }
    }

    pub async fn wait(&self) {
        let Some(period) = self.period else { return };
        let slot = {
            let mut next = self.next.lock().expect("pacer lock");
            let slot = (*next).max(Instant::now());
            *next = slot + period;
            slot
        };
        sleep_until(slot).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::join_all;

    #[tokio::test(start_paused = true)]
    async fn spaces_concurrent_commands_by_the_period() {
        let pacer = Pacer::new(100);
        let start = Instant::now();
        let started = join_all((0..5).map(|_| async {
            pacer.wait().await;
            Instant::now() - start
        }))
        .await;
        let expected: Vec<_> = (0..5).map(|i| Duration::from_millis(10 * i)).collect();
        assert_eq!(started, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_time_does_not_build_up_a_burst() {
        let pacer = Pacer::new(10);
        pacer.wait().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let start = Instant::now();
        pacer.wait().await;
        pacer.wait().await;
        assert_eq!(Instant::now() - start, Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_disables_pacing() {
        let pacer = Pacer::new(0);
        let start = Instant::now();
        for _ in 0..1000 {
            pacer.wait().await;
        }
        assert_eq!(Instant::now(), start);
    }
}

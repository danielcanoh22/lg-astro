//! Periodic HDMI input status refresh, without reloading apps or queueing repeated reads.
//!
//! [`Refresh`] is a small scheduler driven from the SDL loop: it decides when a read is due,
//! keeps at most one request in flight and holds the latest result until the UI is idle enough
//! to apply it without moving controls under the user.
use crate::model::Input;
use std::time::{Duration, Instant};

/// Polling state for the input-only refresh.
///
/// Invariant: at most one read is in flight, and only the newest completed snapshot is kept.
#[derive(Default)]
pub struct Refresh {
    next: Option<Instant>,
    in_flight: bool,
    pending: Option<Vec<Input>>,
}

impl Refresh {
    /// Delay between successful reads.
    const INTERVAL: Duration = Duration::from_secs(2);
    /// Longer back-off after a failed read.
    const RETRY: Duration = Duration::from_secs(10);

    /// Returns `true` when a new read should be started now and marks it in flight.
    ///
    /// While `active` is false the schedule is cleared, so the first read after reactivation
    /// waits one full interval instead of firing immediately.
    pub fn due(&mut self, now: Instant, active: bool) -> bool {
        if !active {
            self.next = None;
            return false;
        }
        if self.in_flight || now < *self.next.get_or_insert(now + Self::INTERVAL) {
            return false;
        }
        self.in_flight = true;
        true
    }

    /// Records the outcome of the in-flight read and schedules the next one.
    ///
    /// A successful snapshot replaces any pending one.
    ///
    /// # Errors
    ///
    /// Returns the read error unchanged so the caller can log it; the next attempt is then
    /// delayed by the retry back-off.
    pub fn completed(
        &mut self,
        now: Instant,
        result: Result<Vec<Input>, String>,
    ) -> Result<(), String> {
        self.in_flight = false;
        self.next = Some(
            now + if result.is_ok() {
                Self::INTERVAL
            } else {
                Self::RETRY
            },
        );
        self.pending = Some(result?);
        Ok(())
    }

    /// Returns the latest snapshot once `ready` is true, keeping it while an interaction is
    /// still in progress.
    pub fn take_pending(&mut self, ready: bool) -> Option<Vec<Input>> {
        ready.then(|| self.pending.take()).flatten()
    }

    /// Drops the pending snapshot and restarts the interval because a later complete
    /// inventory already contains newer input data.
    pub fn superseded(&mut self, now: Instant) {
        self.pending = None;
        self.next = Some(now + Self::INTERVAL);
    }
}

//! Wall-clock time for credential metadata.

use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

/// The current time as Unix seconds.
///
/// Injected, because recording when a credential was stored and used is the only reason this crate
/// reads the clock, and tests need that to be fixed.
pub trait Clock: fmt::Debug + Send + Sync {
    /// Seconds since the Unix epoch.
    fn now(&self) -> u64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        #[expect(
            clippy::disallowed_methods,
            reason = "credential metadata records when a secret was stored and used; it never reaches plans or lockfiles"
        )]
        let now = SystemTime::now();
        now.duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }
}

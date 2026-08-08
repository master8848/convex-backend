//! Deterministic execution support: seeded RNG and virtual clocks.
//!
//! Queries and mutations must be deterministic: replaying a function with the
//! same arguments and transaction timestamp must produce the same result. This
//! module provides the virtual time and seeded randomness injected into guest
//! modules, mirroring the isolate's `ChaCha12Rng` + `unix_timestamp` model.

use std::time::Duration;

use rand_chacha::ChaCha12Rng;
use wasmtime_wasi::clocks::{
    HostMonotonicClock,
    HostWallClock,
};

/// A wall clock frozen at a fixed instant.
///
/// Guests observe the transaction's timestamp rather than the host's clock,
/// so a retried function observes the same time and produces the same result.
#[derive(Clone, Debug)]
pub struct VirtualWallClock {
    now: Duration,
}

impl VirtualWallClock {
    pub fn new(now: Duration) -> Self {
        Self { now }
    }
}

impl HostWallClock for VirtualWallClock {
    fn resolution(&self) -> Duration {
        Duration::from_millis(1)
    }

    fn now(&self) -> Duration {
        self.now
    }
}

/// A monotonic clock frozen at a fixed instant.
///
/// WASI's monotonic clock is also virtualized so guests cannot observe
/// elapsed wall time. Guests that sleep or wait will trap on the wall-clock
/// timeout instead of advancing.
#[derive(Clone, Debug)]
pub struct VirtualMonotonicClock {
    now: u64,
}

impl VirtualMonotonicClock {
    pub fn new(now: Duration) -> Self {
        Self {
            now: u64::try_from(now.as_nanos()).unwrap_or_default(),
        }
    }
}

impl HostMonotonicClock for VirtualMonotonicClock {
    fn resolution(&self) -> u64 {
        1_000_000
    }

    fn now(&self) -> u64 {
        self.now
    }
}

/// A `ChaCha12`-seeded RNG sharing the cipher and seed conventions of the
/// isolate execution path.
pub type DeterministicRng = ChaCha12Rng;

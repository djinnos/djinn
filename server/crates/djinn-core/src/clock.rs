use std::time::{Duration, Instant, SystemTime};

/// A clock abstraction that provides both wall-clock and monotonic time readings.
///
/// This is the approved wall-clock boundary for the djinn server. All production
/// code that needs to read time should use a `Clock` implementation rather than
/// calling `SystemTime::now()` or `Instant::now()` directly. This makes time
/// deterministic in tests and allows the reliability lint ratchet to deny
/// direct wall-clock reads outside this module.
pub trait Clock: Send + Sync {
    /// Return the current wall-clock time.
    fn now(&self) -> SystemTime;

    /// Return the current monotonic time.
    fn now_instant(&self) -> Instant;
}

/// Production clock backed by the operating system.
///
/// This is the only production type allowed to call `SystemTime::now()` and
/// `Instant::now()` directly.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl SystemClock {
    /// Create a new `SystemClock`.
    pub const fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    #[allow(clippy::disallowed_methods)]
    fn now(&self) -> SystemTime {
        // Approved wall-clock boundary: this module is the only production
        // location allowed to call SystemTime::now().
        SystemTime::now()
    }

    #[allow(clippy::disallowed_methods)]
    fn now_instant(&self) -> Instant {
        // Approved wall-clock boundary: this module is the only production
        // location allowed to call Instant::now().
        Instant::now()
    }
}

/// A deterministic test clock that can be constructed with fixed times and
/// advanced manually without sleeping.
///
/// # Example
///
/// ```
/// use std::time::{Duration, SystemTime};
/// use djinn_core::clock::{Clock, TestClock};
///
/// let epoch = SystemTime::UNIX_EPOCH;
/// let clock = TestClock::new(epoch, std::time::Instant::now());
/// assert_eq!(clock.now(), epoch);
///
/// clock.advance_wall(Duration::from_secs(10));
/// assert_eq!(clock.now(), epoch + Duration::from_secs(10));
/// ```
#[derive(Debug)]
pub struct TestClock {
    wall: std::sync::Mutex<SystemTime>,
    mono: std::sync::Mutex<Instant>,
}

impl TestClock {
    /// Create a new `TestClock` with the given initial wall-clock and monotonic
    /// times.
    pub fn new(wall: SystemTime, mono: Instant) -> Self {
        Self {
            wall: std::sync::Mutex::new(wall),
            mono: std::sync::Mutex::new(mono),
        }
    }

    /// Advance the wall-clock time by `duration`.
    pub fn advance_wall(&self, duration: Duration) {
        let mut wall = self.wall.lock().unwrap();
        *wall += duration;
    }

    /// Advance the monotonic time by `duration`.
    pub fn advance_mono(&self, duration: Duration) {
        let mut mono = self.mono.lock().unwrap();
        *mono += duration;
    }

    /// Set the wall-clock time to an absolute value.
    pub fn set_wall(&self, time: SystemTime) {
        let mut wall = self.wall.lock().unwrap();
        *wall = time;
    }

    /// Set the monotonic time to an absolute value.
    pub fn set_mono(&self, instant: Instant) {
        let mut mono = self.mono.lock().unwrap();
        *mono = instant;
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        *self.wall.lock().unwrap()
    }

    fn now_instant(&self) -> Instant {
        *self.mono.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_smoke() {
        let clock = SystemClock::new();
        let t1 = clock.now();
        let _ = clock.now_instant();
        let t2 = clock.now();
        assert!(t2 >= t1, "wall-clock time should not go backwards");
    }

    #[test]
    fn test_clock_fixed_wall() {
        let epoch = SystemTime::UNIX_EPOCH;
        let clock = TestClock::new(epoch, Instant::now());
        assert_eq!(clock.now(), epoch);
    }

    #[test]
    fn test_clock_advance_wall() {
        let epoch = SystemTime::UNIX_EPOCH;
        let clock = TestClock::new(epoch, Instant::now());
        clock.advance_wall(Duration::from_secs(42));
        assert_eq!(clock.now(), epoch + Duration::from_secs(42));
    }

    #[test]
    fn test_clock_set_wall() {
        let epoch = SystemTime::UNIX_EPOCH;
        let later = epoch + Duration::from_secs(100);
        let clock = TestClock::new(epoch, Instant::now());
        clock.set_wall(later);
        assert_eq!(clock.now(), later);
    }

    #[test]
    fn test_clock_fixed_mono() {
        let base = Instant::now();
        let clock = TestClock::new(SystemTime::UNIX_EPOCH, base);
        assert_eq!(clock.now_instant(), base);
    }

    #[test]
    fn test_clock_advance_mono() {
        let base = Instant::now();
        let clock = TestClock::new(SystemTime::UNIX_EPOCH, base);
        clock.advance_mono(Duration::from_millis(500));
        assert_eq!(clock.now_instant(), base + Duration::from_millis(500));
    }

    #[test]
    fn test_clock_set_mono() {
        let base = Instant::now();
        let later = base + Duration::from_secs(7);
        let clock = TestClock::new(SystemTime::UNIX_EPOCH, base);
        clock.set_mono(later);
        assert_eq!(clock.now_instant(), later);
    }

    #[test]
    fn test_clock_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TestClock>();
        assert_send_sync::<SystemClock>();
    }
}

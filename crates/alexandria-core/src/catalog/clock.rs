use chrono::{DateTime, Utc};

/// Time port so retention/unstamped tests can use a fixed clock
/// (Testing Specification §6.2). The domain asks the clock for the current
/// time rather than calling `Utc::now()` directly.
#[allow(async_fn_in_trait)]
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}
use time::{Duration, OffsetDateTime};

pub const ABSOLUTE_TTL: Duration = Duration::hours(12);
pub const IDLE_TTL: Duration = Duration::hours(1);
pub const REFRESH_INTERVAL: Duration = Duration::minutes(5);

pub fn idle_expiry(now: OffsetDateTime, absolute_expiry: OffsetDateTime) -> OffsetDateTime {
    std::cmp::min(now + IDLE_TTL, absolute_expiry)
}

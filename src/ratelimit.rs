//! Per-tenant outbound send rate limit (ToS protection). One tenant per box means a single
//! process-wide limiter is exactly the right granularity — it bounds how fast this WhatsApp account
//! can emit messages, which is what reduces spam-pattern bans on an unofficial (whatsmeow) client.
//!
//! Backed by `governor`'s direct (un-keyed) limiter: a token bucket refilling at
//! `WA_SEND_RATE_PER_MIN` per minute. We expose a tiny `try_acquire` so the rest of the crate need
//! not learn governor's types.

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use std::num::NonZeroU32;

/// A token-bucket limiter sized to a per-minute send cap.
pub struct SendLimiter {
    inner: DefaultDirectRateLimiter,
}

impl SendLimiter {
    /// Build a limiter permitting `per_minute` sends per minute (with burst = the same value).
    /// A zero rate is clamped to 1 so the limiter is always constructable.
    pub fn per_minute(per_minute: u32) -> Self {
        let quota = Quota::per_minute(NonZeroU32::new(per_minute.max(1)).expect("clamped to >= 1"));
        Self {
            inner: RateLimiter::direct(quota),
        }
    }

    /// Try to consume one token. `true` = permitted (send now); `false` = over the limit, the
    /// caller should reject with 429 rather than send.
    pub fn try_acquire(&self) -> bool {
        self.inner.check().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_up_to_the_burst_then_blocks() {
        let limiter = SendLimiter::per_minute(3);
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
        // Bucket drained within the same minute.
        assert!(!limiter.try_acquire());
    }
}

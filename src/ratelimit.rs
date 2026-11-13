//! Discord's rate limits, kept from the headers of each response: a bucket
//! per route (shared by the routes Discord says share one) and the global
//! limit. A request waits here rather than being sent into a 429.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::lock;

#[derive(Default)]
pub struct RateLimiter {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// No request goes out before this.
    global_until: Option<Instant>,
    /// Route key to the bucket Discord put it in.
    bucket_of: HashMap<String, String>,
    buckets: HashMap<String, Bucket>,
}

struct Bucket {
    remaining: u64,
    reset_at: Instant,
}

/// What a response said about its limits.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Observed {
    /// `X-RateLimit-Bucket`.
    pub bucket: Option<String>,
    /// `X-RateLimit-Remaining`.
    pub remaining: Option<u64>,
    /// `X-RateLimit-Reset-After`, in seconds.
    pub reset_after: Option<f64>,
}

impl RateLimiter {
    /// Take a slot for a request on `route`, or learn how long to wait for one.
    pub fn claim(&self, route: &str, now: Instant) -> Result<(), Duration> {
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        if let Some(until) = state.global_until {
            if until > now {
                return Err(until - now);
            }
            state.global_until = None;
        }
        let Some(id) = state.bucket_of.get(route) else {
            // Nothing known yet: the first response tells.
            return Ok(());
        };
        let Some(bucket) = state.buckets.get_mut(id) else {
            return Ok(());
        };
        if now >= bucket.reset_at {
            state.buckets.remove(id);
            return Ok(());
        }
        if bucket.remaining == 0 {
            return Err(bucket.reset_at - now);
        }
        bucket.remaining -= 1;
        Ok(())
    }

    /// Record what a response's headers said.
    pub fn observe(&self, route: &str, seen: &Observed, now: Instant) {
        let (Some(bucket), Some(remaining), Some(reset_after)) = (&seen.bucket, seen.remaining, seen.reset_after)
        else {
            return;
        };
        // Routes sharing a bucket still count separately per major parameter, which the route key carries.
        let id = format!("{bucket}|{}", major(route));
        let mut state = lock(&self.state);
        state.bucket_of.insert(route.to_owned(), id.clone());
        let reset_at = now + Duration::from_secs_f64(reset_after.max(0.0));
        match state.buckets.get_mut(&id) {
            // Responses can arrive out of order: within one window the lower count is the truth.
            Some(known) if known.reset_at > now && reset_at <= known.reset_at + Duration::from_millis(500) => {
                known.remaining = known.remaining.min(remaining);
            }
            _ => {
                state.buckets.insert(id, Bucket { remaining, reset_at });
            }
        }
    }

    /// Record a 429: nothing on `route` (or anywhere, when `global`) for `retry_after`.
    pub fn limited(&self, route: &str, retry_after: Duration, global: bool, now: Instant) {
        let mut state = lock(&self.state);
        let until = now + retry_after;
        if global {
            // `None` orders before any time, so this keeps the later of the two.
            state.global_until = state.global_until.max(Some(until));
            return;
        }
        let id = state
            .bucket_of
            .entry(route.to_owned())
            .or_insert_with(|| format!("?|{route}"))
            .clone();
        state.buckets.insert(
            id,
            Bucket {
                remaining: 0,
                reset_at: until,
            },
        );
    }
}

/// The ids in a route key: what separates one channel's bucket from another's.
fn major(route: &str) -> String {
    route
        .split('/')
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(bucket: &str, remaining: u64, reset_after: f64) -> Observed {
        Observed {
            bucket: Some(bucket.into()),
            remaining: Some(remaining),
            reset_after: Some(reset_after),
        }
    }

    #[test]
    fn an_unknown_route_goes_straight_through() {
        let limiter = RateLimiter::default();
        assert_eq!(limiter.claim("GET/users/@me", Instant::now()), Ok(()));
    }

    #[test]
    fn a_spent_bucket_waits_for_its_reset() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let route = "POST/channels/1/messages";
        limiter.observe(route, &seen("abc", 1, 2.0), now);
        assert_eq!(limiter.claim(route, now), Ok(()));
        assert_eq!(limiter.claim(route, now), Err(Duration::from_secs(2)));
        assert_eq!(limiter.claim(route, now + Duration::from_secs(3)), Ok(()));
    }

    #[test]
    fn routes_in_one_bucket_share_it_but_channels_do_not() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        limiter.observe("GET/channels/1/messages", &seen("abc", 1, 5.0), now);
        limiter.observe("GET/channels/1/pins", &seen("abc", 1, 5.0), now);
        limiter.observe("GET/channels/2/messages", &seen("abc", 1, 5.0), now);
        assert_eq!(limiter.claim("GET/channels/1/messages", now), Ok(()));
        assert!(limiter.claim("GET/channels/1/pins", now).is_err());
        assert_eq!(limiter.claim("GET/channels/2/messages", now), Ok(()));
    }

    #[test]
    fn a_late_response_does_not_give_slots_back() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let route = "GET/guilds/1/roles";
        limiter.observe(route, &seen("abc", 0, 5.0), now);
        limiter.observe(route, &seen("abc", 3, 5.0), now);
        assert!(limiter.claim(route, now).is_err());
    }

    #[test]
    fn a_429_blocks_the_route_and_a_global_one_blocks_everything() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        limiter.limited("GET/a", Duration::from_secs(4), false, now);
        assert_eq!(limiter.claim("GET/a", now), Err(Duration::from_secs(4)));
        assert_eq!(limiter.claim("GET/b", now), Ok(()));
        limiter.limited("GET/a", Duration::from_secs(9), true, now);
        assert_eq!(limiter.claim("GET/b", now), Err(Duration::from_secs(9)));
        assert_eq!(limiter.claim("GET/b", now + Duration::from_secs(10)), Ok(()));
    }
}

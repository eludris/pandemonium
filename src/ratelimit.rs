use std::{
    fmt::Display,
    time::{Duration, SystemTime},
};

use deadpool_redis::{redis::AsyncCommands, Connection};

/// A simple Ratelimiter than can keep track of ratelimit data from KeyDB and add ratelimit
/// related headers to a response type
pub struct Ratelimiter {
    cache: Connection,
    key: String,
    reset_after: Duration,
    request_limit: u32,
    request_count: u32,
    last_reset: u64,
}

impl Ratelimiter {
    /// Creates a new Ratelimiter
    pub fn new<I>(
        cache: Connection,
        identifier: I,
        reset_after: Duration,
        request_limit: u32,
    ) -> Ratelimiter
    where
        I: Display,
    {
        Ratelimiter {
            cache,
            key: format!("ratelimit:pandemonium:{}", identifier),
            reset_after,
            request_limit,
            request_count: 0,
            last_reset: 0,
        }
    }

    /// Checks if a bucket is ratelimited, if so returns an Error with an ErrorResponse
    pub async fn process_ratelimit(&mut self) -> Result<(), ()> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as u64;

        if let (Some(last_reset), Some(request_count)) = self
            .cache
            .hget::<&str, (&str, &str), (Option<u64>, Option<u32>)>(
                &self.key,
                ("last_reset", "request_count"),
            )
            .await
            .expect("Coudln't query cache")
        {
            self.last_reset = last_reset;
            self.request_count = request_count;
            if now - self.last_reset >= self.reset_after.as_millis() as u64 {
                self.cache.del::<&str, ()>(&self.key).await.unwrap();
                self.cache
                    .hset_multiple::<&str, &str, u64, ()>(
                        &self.key,
                        &[("last_reset", now), ("request_count", 0)],
                    )
                    .await
                    .unwrap();
                self.last_reset = now;
                self.request_count = 0;
                log::debug!("Reset bucket for {}", self.key);
            }
            if self.request_count >= self.request_limit {
                log::info!("Ratelimited bucket {}", self.key);
                Err(())
            } else {
                self.cache
                    .hincr::<&str, &str, u8, ()>(&self.key, "request_count", 1)
                    .await
                    .unwrap();
                self.request_count += 1;
                Ok(())
            }
        } else {
            log::debug!("New bucket for {}", self.key);
            self.cache
                .hset_multiple::<&str, &str, u64, ()>(
                    &self.key,
                    &[("last_reset", now), ("request_count", 1)],
                )
                .await
                .unwrap();
            Ok(())
        }
    }

    pub async fn clear_bucket(mut self) {
        self.cache.del(self.key).await.unwrap()
    }
}

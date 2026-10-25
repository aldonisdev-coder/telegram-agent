use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

pub struct TokenBucket {
    capacity: u32,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            refill_per_sec,
            last: Instant::now(),
        }
    }
    pub fn try_consume(&mut self, n: u32) -> Result<(), u64> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity as f64);
        self.last = now;
        if self.tokens >= n as f64 {
            self.tokens -= n as f64;
            Ok(())
        } else {
            let need = (n as f64 - self.tokens) / self.refill_per_sec;
            Err(need.ceil() as u64)
        }
    }
}

pub struct RateLimiter {
    buckets: Mutex<HashMap<i64, TokenBucket>>,
    capacity: u32,
    refill_per_sec: f64,
}

impl RateLimiter {
    pub fn new(rpm: u32) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            capacity: rpm,
            refill_per_sec: rpm as f64 / 60.0,
        }
    }
    pub fn check(&self, seller_id: i64) -> Result<(), u64> {
        let mut map = self.buckets.lock().unwrap();
        let b = map
            .entry(seller_id)
            .or_insert_with(|| TokenBucket::new(self.capacity, self.refill_per_sec));
        b.try_consume(1)
    }
}

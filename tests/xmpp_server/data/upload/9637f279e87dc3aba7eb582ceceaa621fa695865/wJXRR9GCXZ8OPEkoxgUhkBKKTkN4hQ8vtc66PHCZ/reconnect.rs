use std::time::Duration;

/// Exponential backoff for reconnects: 1 s, 2 s, 4 s ... capped at 5 minutes, with jitter.
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub fn new() -> Self {
        Self { attempt: 0 }
    }

    pub fn next(&mut self) -> Duration {
        let base = 1u64 << self.attempt.min(8);
        self.attempt += 1;
        let jitter = fastrand::u64(0..=base * 250);
        Duration::from_millis((base * 1000 + jitter).min(300_000))
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_and_caps() {
        let mut b = Backoff::new();
        assert!(b.next() >= Duration::from_secs(1));
        for _ in 0..20 {
            b.next();
        }
        assert!(b.next() <= Duration::from_secs(300));
    }
}

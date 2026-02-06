//! Per-peer token-bucket rate limiter for presence messages

use saorsa_gossip_types::PeerId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Configuration for rate limiting
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum tokens (burst capacity)
    pub max_tokens: f64,
    /// Token refill rate per second
    pub refill_rate: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_tokens: 3.0,  // Allow burst of 3
            refill_rate: 0.2, // 12 per minute = 0.2 per second
        }
    }
}

/// Token bucket for a single peer
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(max_tokens: f64, refill_rate: f64) -> Self {
        Self {
            tokens: max_tokens, // Start with full bucket
            max_tokens,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();

        // Add tokens based on elapsed time
        let tokens_to_add = elapsed * self.refill_rate;
        self.tokens = (self.tokens + tokens_to_add).min(self.max_tokens);

        self.last_refill = now;
    }

    /// Try to consume one token
    ///
    /// Returns true if the token was consumed, false if rate limited.
    fn try_consume(&mut self) -> bool {
        self.refill();

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Per-peer rate limiter using token bucket algorithm
#[derive(Debug)]
pub struct RateLimiter {
    buckets: HashMap<PeerId, TokenBucket>,
    config: RateLimitConfig,
}

impl RateLimiter {
    /// Create a new rate limiter with the given configuration
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            buckets: HashMap::new(),
            config,
        }
    }

    /// Check if a message from a peer should be allowed
    ///
    /// Returns true if the message is allowed, false if rate limited.
    /// Automatically refills tokens based on elapsed time.
    pub fn check_rate_limit(&mut self, peer: PeerId) -> bool {
        let bucket = self
            .buckets
            .entry(peer)
            .or_insert_with(|| TokenBucket::new(self.config.max_tokens, self.config.refill_rate));

        bucket.try_consume()
    }

    /// Remove stale entries not accessed for the given duration
    ///
    /// Returns the number of entries removed.
    pub fn cleanup_stale(&mut self, max_age: Duration) -> usize {
        let initial_count = self.buckets.len();
        let now = Instant::now();

        self.buckets
            .retain(|_, bucket| now.duration_since(bucket.last_refill) <= max_age);

        initial_count - self.buckets.len()
    }

    /// Get the number of tracked peers
    pub fn peer_count(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = RateLimitConfig::default();
        assert_eq!(config.max_tokens, 3.0);
        assert_eq!(config.refill_rate, 0.2);
    }

    #[test]
    fn test_allows_within_limit() {
        let mut limiter = RateLimiter::new(RateLimitConfig::default());
        let peer = PeerId::new([1u8; 32]);

        // Should allow first 3 requests (burst capacity)
        assert!(
            limiter.check_rate_limit(peer),
            "First request should be allowed"
        );
        assert!(
            limiter.check_rate_limit(peer),
            "Second request should be allowed"
        );
        assert!(
            limiter.check_rate_limit(peer),
            "Third request should be allowed"
        );
    }

    #[test]
    fn test_blocks_over_limit() {
        let mut limiter = RateLimiter::new(RateLimitConfig::default());
        let peer = PeerId::new([1u8; 32]);

        // Consume all tokens
        assert!(limiter.check_rate_limit(peer));
        assert!(limiter.check_rate_limit(peer));
        assert!(limiter.check_rate_limit(peer));

        // Next request should be blocked
        assert!(
            !limiter.check_rate_limit(peer),
            "Fourth request should be blocked"
        );
    }

    #[test]
    fn test_tokens_refill_over_time() {
        let config = RateLimitConfig {
            max_tokens: 3.0,
            refill_rate: 10.0, // Fast refill for testing
        };
        let mut limiter = RateLimiter::new(config);
        let peer = PeerId::new([1u8; 32]);

        // Consume all tokens
        assert!(limiter.check_rate_limit(peer));
        assert!(limiter.check_rate_limit(peer));
        assert!(limiter.check_rate_limit(peer));
        assert!(!limiter.check_rate_limit(peer));

        // Wait for refill (0.1 second should give us 1 token at 10 tokens/sec)
        std::thread::sleep(std::time::Duration::from_millis(110));

        // Should now be allowed again
        assert!(
            limiter.check_rate_limit(peer),
            "Should be allowed after refill"
        );
    }

    #[test]
    fn test_cleanup_removes_stale() {
        let mut limiter = RateLimiter::new(RateLimitConfig::default());
        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);

        // Access both peers
        limiter.check_rate_limit(peer1);
        limiter.check_rate_limit(peer2);

        assert_eq!(limiter.peer_count(), 2);

        // Wait a bit
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Access only peer1
        limiter.check_rate_limit(peer1);

        // Cleanup stale entries (50ms max age)
        let removed = limiter.cleanup_stale(Duration::from_millis(50));

        // peer2 should be removed (not accessed recently)
        assert_eq!(removed, 1);
        assert_eq!(limiter.peer_count(), 1);
    }

    #[test]
    fn test_different_peers_independent() {
        let mut limiter = RateLimiter::new(RateLimitConfig::default());
        let peer1 = PeerId::new([1u8; 32]);
        let peer2 = PeerId::new([2u8; 32]);

        // Exhaust peer1's tokens
        assert!(limiter.check_rate_limit(peer1));
        assert!(limiter.check_rate_limit(peer1));
        assert!(limiter.check_rate_limit(peer1));
        assert!(!limiter.check_rate_limit(peer1));

        // peer2 should still have tokens
        assert!(
            limiter.check_rate_limit(peer2),
            "peer2 should have independent bucket"
        );
        assert!(limiter.check_rate_limit(peer2));
        assert!(limiter.check_rate_limit(peer2));
    }

    #[test]
    fn test_peer_count() {
        let mut limiter = RateLimiter::new(RateLimitConfig::default());

        assert_eq!(limiter.peer_count(), 0, "Should start with no peers");

        limiter.check_rate_limit(PeerId::new([1u8; 32]));
        assert_eq!(limiter.peer_count(), 1);

        limiter.check_rate_limit(PeerId::new([2u8; 32]));
        assert_eq!(limiter.peer_count(), 2);

        // Same peer doesn't increase count
        limiter.check_rate_limit(PeerId::new([1u8; 32]));
        assert_eq!(limiter.peer_count(), 2);
    }
}

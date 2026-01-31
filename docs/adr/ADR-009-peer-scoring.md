# ADR-009: Peer Scoring Architecture

## Status

Accepted (2025-12-24)

## Context

In gossip networks, not all peers provide equal quality service:

| Peer Behavior | Impact |
|---------------|--------|
| High latency | Slows message delivery |
| Frequent failures | Wastes probe resources |
| Invalid messages | Spam, potential attacks |
| Duplicate flooding | Bandwidth waste |
| Good performance | Reliable, fast delivery |

Treating all peers equally leads to:
- Suboptimal routing through slow paths
- Wasted resources on unreliable peers
- Vulnerability to low-quality or malicious nodes

We needed a peer quality system that:
- Tracks observable behavior metrics
- Informs routing and neighbor selection
- Adapts to changing conditions
- Resists gaming/manipulation

## Decision

Implement **multi-metric peer scoring** that influences protocol decisions:

### Score Components

```rust
/// Per-peer quality metrics
#[derive(Debug, Clone, Default)]
pub struct PeerScore {
    /// Message delivery latency statistics
    pub delivery_latency: LatencyStats,

    /// Connection reliability
    pub reliability: ReliabilityStats,

    /// Protocol compliance
    pub compliance: ComplianceStats,

    /// Overall computed score (-1000 to +1000)
    pub score: i32,

    /// Last update timestamp
    pub last_updated: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LatencyStats {
    /// P50 delivery latency (ms)
    pub p50: u64,
    /// P95 delivery latency (ms)
    pub p95: u64,
    /// P99 delivery latency (ms)
    pub p99: u64,
    /// Sample count
    pub samples: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ReliabilityStats {
    /// Successful connections
    pub connection_successes: u64,
    /// Failed connection attempts
    pub connection_failures: u64,
    /// SWIM probe responses
    pub probe_responses: u64,
    /// SWIM probe timeouts
    pub probe_timeouts: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ComplianceStats {
    /// Valid messages received
    pub valid_messages: u64,
    /// Invalid/malformed messages
    pub invalid_messages: u64,
    /// Excessive duplicate messages
    pub duplicate_floods: u64,
    /// Rate limit violations
    pub rate_violations: u64,
}
```

### Score Calculation

```rust
impl PeerScore {
    /// Calculate overall score from component metrics
    pub fn calculate(&mut self) {
        let mut score = 0i32;

        // Latency component (-200 to +200)
        score += self.latency_score();

        // Reliability component (-300 to +300)
        score += self.reliability_score();

        // Compliance component (-500 to +500)
        score += self.compliance_score();

        self.score = score.clamp(-1000, 1000);
        self.last_updated = unix_time_millis();
    }

    fn latency_score(&self) -> i32 {
        if self.delivery_latency.samples < 10 {
            return 0; // Not enough data
        }

        // Good: P95 < 200ms
        // Neutral: P95 200-500ms
        // Bad: P95 > 500ms
        match self.delivery_latency.p95 {
            0..=100 => 200,
            101..=200 => 100,
            201..=500 => 0,
            501..=1000 => -100,
            _ => -200,
        }
    }

    fn reliability_score(&self) -> i32 {
        let total_conns = self.reliability.connection_successes
            + self.reliability.connection_failures;

        if total_conns < 5 {
            return 0; // Not enough data
        }

        let conn_rate = self.reliability.connection_successes as f64 / total_conns as f64;

        let total_probes = self.reliability.probe_responses
            + self.reliability.probe_timeouts;

        let probe_rate = if total_probes > 0 {
            self.reliability.probe_responses as f64 / total_probes as f64
        } else {
            1.0
        };

        let combined_rate = (conn_rate + probe_rate) / 2.0;

        // Map 0.0-1.0 to -300..+300
        ((combined_rate * 600.0) - 300.0) as i32
    }

    fn compliance_score(&self) -> i32 {
        // Heavy penalty for invalid messages
        let invalid_penalty = (self.compliance.invalid_messages * 50) as i32;

        // Moderate penalty for floods
        let flood_penalty = (self.compliance.duplicate_floods * 10) as i32;

        // Minor penalty for rate violations
        let rate_penalty = (self.compliance.rate_violations * 5) as i32;

        // Bonus for valid messages (up to 500)
        let valid_bonus = ((self.compliance.valid_messages / 100) as i32).min(500);

        valid_bonus - invalid_penalty - flood_penalty - rate_penalty
    }
}
```

### Score Decay

Scores decay over time to adapt to changing conditions:

```rust
impl PeerScorer {
    const DECAY_INTERVAL: Duration = Duration::from_secs(3600); // 1 hour
    const DECAY_FACTOR: f64 = 0.9;

    /// Apply periodic decay to all scores
    fn decay_scores(&mut self) {
        let now = unix_time_millis();

        for score in self.scores.values_mut() {
            let age_hours = (now - score.last_updated) / 3600000;

            if age_hours > 0 {
                // Decay positive scores toward 0
                // Decay negative scores toward 0
                let decay = Self::DECAY_FACTOR.powi(age_hours as i32);
                score.score = (score.score as f64 * decay) as i32;
            }
        }
    }
}
```

### Protocol Integration

#### 1. HyParView Neighbor Selection

```rust
impl HyParView {
    /// Select peers for active view promotion
    fn select_for_promotion(&self, candidates: &[PeerId]) -> Vec<PeerId> {
        let mut scored: Vec<_> = candidates
            .iter()
            .map(|p| (p, self.scorer.get_score(p)))
            .collect();

        // Sort by score (highest first)
        scored.sort_by(|a, b| b.1.cmp(&a.1));

        // Take top candidates, ensuring diversity
        let mut selected = Vec::new();
        let mut seen_prefixes = HashSet::new();

        for (peer, score) in scored {
            if score < -500 {
                continue; // Skip very low-scoring peers
            }

            // Ensure IP prefix diversity
            let prefix = self.get_ip_prefix(peer);
            if seen_prefixes.insert(prefix) {
                selected.push(*peer);
            }

            if selected.len() >= self.config.active_view_size {
                break;
            }
        }

        selected
    }
}
```

#### 2. Plumtree Eager/Lazy Selection

```rust
impl Plumtree {
    /// Decide whether peer should be eager or lazy
    fn should_be_eager(&self, peer: &PeerId) -> bool {
        let score = self.scorer.get_score(peer);

        // High-scoring peers preferred for eager tree
        if score > 200 {
            return true;
        }

        // Low-scoring peers kept lazy
        if score < -200 {
            return false;
        }

        // Random for middle scores
        rand::random::<bool>()
    }

    /// Handle duplicate message (potential PRUNE)
    fn on_duplicate(&mut self, peer: PeerId) {
        // Record duplicate event
        self.scorer.record_duplicate(&peer);

        // Consider PRUNE if too many duplicates
        if self.scorer.duplicate_rate(&peer) > 0.3 {
            self.send_prune(peer);
            self.move_to_lazy(peer);
        }
    }
}
```

#### 3. Bootstrap Peer Selection

```rust
impl BootstrapManager {
    /// Select bootstrap peers (roles are hints only)
    fn select_bootstrap_peers(&self, count: usize) -> Vec<SocketAddr> {
        let mut candidates: Vec<_> = self.peer_cache
            .iter()
            .map(|p| {
                let hint_bonus = if p.roles.coordinator { 50 } else { 0 };
                (p, self.scorer.get_score(&p.peer_id) + hint_bonus)
            })
            .collect();

        // Epsilon-greedy selection
        let epsilon = 0.1;
        let mut selected = Vec::new();

        for _ in 0..count {
            if rand::random::<f64>() < epsilon || candidates.is_empty() {
                // Explore: random selection
                if let Some(idx) = (0..candidates.len()).choose(&mut rand::thread_rng()) {
                    selected.push(candidates.remove(idx));
                }
            } else {
                // Exploit: best scoring
                candidates.sort_by(|a, b| b.1.cmp(&a.1));
                selected.push(candidates.remove(0));
            }
        }

        selected.into_iter().map(|(p, _)| p.addr_hints[0]).collect()
    }
}
```

### Recording Events

```rust
impl PeerScorer {
    /// Record successful message delivery
    pub fn record_delivery(&mut self, peer: &PeerId, latency_ms: u64) {
        let score = self.scores.entry(*peer).or_default();
        score.delivery_latency.add_sample(latency_ms);
        score.calculate();
    }

    /// Record connection outcome
    pub fn record_connection(&mut self, peer: &PeerId, success: bool) {
        let score = self.scores.entry(*peer).or_default();
        if success {
            score.reliability.connection_successes += 1;
        } else {
            score.reliability.connection_failures += 1;
        }
        score.calculate();
    }

    /// Record message validity
    pub fn record_message(&mut self, peer: &PeerId, valid: bool) {
        let score = self.scores.entry(*peer).or_default();
        if valid {
            score.compliance.valid_messages += 1;
        } else {
            score.compliance.invalid_messages += 1;
        }
        score.calculate();
    }

    /// Record SWIM probe outcome
    pub fn record_probe(&mut self, peer: &PeerId, responded: bool) {
        let score = self.scores.entry(*peer).or_default();
        if responded {
            score.reliability.probe_responses += 1;
        } else {
            score.reliability.probe_timeouts += 1;
        }
        score.calculate();
    }
}
```

### Persistence

Scores are persisted to survive restarts:

```rust
impl PeerScorer {
    pub fn save(&self, path: &Path) -> Result<()> {
        let data: HashMap<_, _> = self.scores
            .iter()
            .filter(|(_, s)| s.score.abs() > 100) // Only save significant scores
            .map(|(k, v)| (*k, v.clone()))
            .collect();

        let file = File::create(path)?;
        ciborium::into_writer(&data, BufWriter::new(file))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }

        let file = File::open(path)?;
        let scores = ciborium::from_reader(BufReader::new(file))?;
        Ok(Self { scores, ..Default::default() })
    }
}
```

## Consequences

### Benefits

1. **Quality-aware routing**: Prefer reliable, fast peers
2. **Attack mitigation**: Penalize misbehaving peers
3. **Self-optimizing**: Network improves over time
4. **Resource efficiency**: Avoid wasting resources on bad peers
5. **Adaptive**: Responds to changing peer behavior

### Trade-offs

1. **Cold start**: New peers have no score data
2. **Gaming potential**: Peers can try to inflate scores
3. **False positives**: Network issues may lower scores unfairly
4. **Storage**: Scores must be persisted per peer

### Anti-Gaming Measures

```rust
impl PeerScorer {
    /// Prevent score inflation attacks
    fn validate_event(&self, peer: &PeerId, event: &ScoreEvent) -> bool {
        match event {
            // Latency can't be self-reported
            ScoreEvent::Delivery { .. } => true, // Measured locally

            // Rate limit connection attempts
            ScoreEvent::Connection { .. } => {
                self.connection_rate(peer) < 10 // Max 10/minute
            }

            // Cross-check message validity
            ScoreEvent::Message { .. } => true, // Signature verified

            _ => true,
        }
    }
}
```

## Alternatives Considered

### 1. Binary Good/Bad Classification

Simple two-state peer classification.

**Rejected because**:
- No nuance for partially good peers
- Doesn't capture latency differences
- Can't recover from temporary issues

### 2. Reputation Systems

Third-party attestations of peer quality.

**Rejected because**:
- Sybil attacks on attestors
- Adds protocol complexity
- Centralization risks

### 3. No Scoring (Random Selection)

Treat all peers equally.

**Rejected because**:
- Wastes resources on bad peers
- Vulnerable to low-quality attacks
- Suboptimal routing

### 4. Pure Latency-Based

Only consider message latency.

**Rejected because**:
- Ignores reliability
- Ignores compliance
- Easy to game with fast spam

## References

- **Implementation**: `crates/membership/src/scoring.rs`
- **Integration**: HyParView (`crates/membership/`), Plumtree (`crates/pubsub/`)
- **Persistence**: Score cache in `~/.saorsa-gossip/peer_scores.bin`
- **Related**: ADR-001 (Protocol Layering uses scores for neighbor selection)

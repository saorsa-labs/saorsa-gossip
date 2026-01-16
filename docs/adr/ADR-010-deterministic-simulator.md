# ADR-010: Deterministic Network Simulator

## Status

Retired (2026-01-16)

> **Why retired?** We now mandate end-to-end testing over real ant-quic transports, even in CI. The deterministic simulator and mock transports were removed from the repository, but this ADR is preserved to capture the historical design.

## Context

Testing gossip protocols requires validating behavior under:

| Condition | Challenge |
|-----------|-----------|
| Network partitions | Hard to create reliably |
| Message loss | Non-deterministic by nature |
| Latency variations | Real networks unpredictable |
| Node failures | Timing-sensitive |
| Concurrent events | Race conditions |

Traditional testing approaches have limitations:

| Approach | Problem |
|----------|---------|
| Unit tests | Don't capture distributed behavior |
| Integration tests | Slow, non-deterministic |
| Real network tests | Expensive, hard to reproduce |
| Docker/k8s tests | Still non-deterministic timing |

We needed a simulator that:
- Reproduces exact test conditions
- Controls time precisely
- Injects chaos deterministically
- Runs quickly (not real-time)
- Enables property-based testing

## Decision

Build a **deterministic network simulator** with controlled randomness and virtual time:

### Core Architecture

```rust
/// Deterministic network simulator
pub struct NetworkSimulator {
    /// Simulated nodes
    nodes: Vec<SimulatedNode>,

    /// Network topology and links
    topology: Topology,
    link_configs: HashMap<(NodeId, NodeId), LinkConfig>,

    /// Message queue (ordered by delivery time)
    message_queue: BinaryHeap<ScheduledMessage>,

    /// Virtual clock
    virtual_time: Duration,

    /// Deterministic RNG
    rng: Pcg64,

    /// Chaos injector for failure scenarios
    chaos: ChaosInjector,

    /// Running state
    running: bool,
}

/// Node identifier in simulation
pub type NodeId = usize;

/// Simulated network node
pub struct SimulatedNode {
    pub id: NodeId,
    pub peer_id: PeerId,
    pub state: NodeState,
    pub inbox: VecDeque<SimulatedMessage>,
}
```

### Deterministic Randomness

All randomness uses seeded PCG generator:

```rust
impl NetworkSimulator {
    /// Create with fixed seed for reproducibility
    pub fn new() -> Self {
        Self::with_seed(42) // Default seed
    }

    pub fn with_seed(seed: u64) -> Self {
        Self {
            rng: Pcg64::seed_from_u64(seed),
            // ...
        }
    }

    /// Generate random value deterministically
    fn random<T: SampleUniform>(&mut self, range: Range<T>) -> T {
        self.rng.gen_range(range)
    }
}
```

**Key property**: Same seed → same test execution → reproducible bugs.

### Virtual Time

Time advances discretely based on scheduled events:

```rust
impl NetworkSimulator {
    /// Advance simulation time
    pub fn tick(&mut self) -> Result<(), SimError> {
        // Find next event time
        let next_time = self.message_queue
            .peek()
            .map(|m| m.delivery_time)
            .unwrap_or(self.virtual_time + Duration::from_millis(1));

        // Advance clock
        self.virtual_time = next_time;

        // Deliver messages scheduled for now
        while let Some(msg) = self.message_queue.peek() {
            if msg.delivery_time > self.virtual_time {
                break;
            }
            let msg = self.message_queue.pop().unwrap();
            self.deliver_message(msg)?;
        }

        Ok(())
    }

    /// Run simulation for duration
    pub async fn run_for(&mut self, duration: Duration) -> Result<SimStats, SimError> {
        let end_time = self.virtual_time + duration;

        while self.virtual_time < end_time && self.running {
            self.tick()?;
        }

        Ok(self.collect_stats())
    }
}
```

**Benefit**: 1 hour of simulated time can run in seconds.

### Topology Support

```rust
#[derive(Debug, Clone)]
pub enum Topology {
    /// All nodes connected to all others
    Mesh,
    /// One central node, others connect to it
    Star { center: NodeId },
    /// Nodes connected in a ring
    Ring,
    /// Hierarchical tree structure
    Tree { branching_factor: usize },
    /// Custom adjacency specification
    Custom { adjacency: Vec<(NodeId, NodeId)> },
}

impl NetworkSimulator {
    /// Configure network topology
    pub fn with_topology(mut self, topology: Topology) -> Self {
        self.topology = topology;
        self.initialize_links();
        self
    }

    fn initialize_links(&mut self) {
        match &self.topology {
            Topology::Mesh => {
                for i in 0..self.nodes.len() {
                    for j in (i + 1)..self.nodes.len() {
                        self.create_link(i, j);
                    }
                }
            }
            Topology::Ring => {
                for i in 0..self.nodes.len() {
                    let next = (i + 1) % self.nodes.len();
                    self.create_link(i, next);
                }
            }
            // ... other topologies
        }
    }
}
```

### Link Configuration

```rust
#[derive(Debug, Clone)]
pub struct LinkConfig {
    /// Base latency (ms)
    pub latency_ms: u64,
    /// Latency jitter (±ms)
    pub jitter_ms: u64,
    /// Bandwidth limit (bytes/sec)
    pub bandwidth_bps: u64,
    /// Packet loss probability (0.0-1.0)
    pub packet_loss_rate: f64,
    /// Link enabled/disabled
    pub enabled: bool,
}

impl NetworkSimulator {
    /// Configure specific link
    pub fn set_link_config(&mut self, a: NodeId, b: NodeId, config: LinkConfig) {
        self.link_configs.insert((a.min(b), a.max(b)), config);
    }

    /// Configure all links uniformly
    pub fn set_link_config_all(&mut self, config: LinkConfig) {
        for link in self.link_configs.values_mut() {
            *link = config.clone();
        }
    }
}
```

### Message Delivery

```rust
#[derive(Debug, Clone)]
pub struct SimulatedMessage {
    pub from: NodeId,
    pub to: NodeId,
    pub message_type: MessageType,
    pub payload: Vec<u8>,
    pub sent_at: Duration,
}

#[derive(Debug)]
struct ScheduledMessage {
    message: SimulatedMessage,
    delivery_time: Duration,
}

impl NetworkSimulator {
    /// Send message through simulated network
    pub async fn send_message(
        &mut self,
        from: NodeId,
        to: NodeId,
        payload: Vec<u8>,
        message_type: MessageType,
    ) -> Result<(), SimError> {
        let link_key = (from.min(to), from.max(to));
        let config = self.link_configs.get(&link_key)
            .ok_or(SimError::NoLink(from, to))?;

        // Check if link is enabled
        if !config.enabled {
            return Err(SimError::LinkDisabled(from, to));
        }

        // Apply packet loss
        if self.random(0.0..1.0) < config.packet_loss_rate {
            return Ok(()); // Message dropped
        }

        // Calculate delivery time with jitter
        let jitter = self.random(0..config.jitter_ms * 2) as i64
            - config.jitter_ms as i64;
        let latency = (config.latency_ms as i64 + jitter).max(1) as u64;

        let delivery_time = self.virtual_time + Duration::from_millis(latency);

        self.message_queue.push(ScheduledMessage {
            message: SimulatedMessage {
                from,
                to,
                message_type,
                payload,
                sent_at: self.virtual_time,
            },
            delivery_time,
        });

        Ok(())
    }
}
```

### Chaos Injection

```rust
/// Chaos engineering capabilities
pub struct ChaosInjector {
    /// Scheduled failures
    failures: Vec<ScheduledFailure>,
    /// Active partitions
    partitions: Vec<Partition>,
}

#[derive(Debug)]
pub struct ScheduledFailure {
    pub node: NodeId,
    pub at_time: Duration,
    pub failure_type: FailureType,
}

#[derive(Debug)]
pub enum FailureType {
    /// Node crashes (stops processing)
    Crash,
    /// Node restarts after duration
    CrashRecover { after: Duration },
    /// Node becomes Byzantine (sends invalid messages)
    Byzantine,
    /// Node becomes slow (increased latency)
    Slowdown { factor: f64 },
}

#[derive(Debug)]
pub struct Partition {
    /// Nodes in partition A
    pub group_a: HashSet<NodeId>,
    /// Nodes in partition B
    pub group_b: HashSet<NodeId>,
    /// When partition starts
    pub start: Duration,
    /// When partition heals (None = permanent)
    pub end: Option<Duration>,
}

impl ChaosInjector {
    /// Schedule node failure
    pub fn inject_failure(&mut self, node: NodeId, at: Duration, failure: FailureType) {
        self.failures.push(ScheduledFailure {
            node,
            at_time: at,
            failure_type: failure,
        });
    }

    /// Create network partition
    pub fn partition(
        &mut self,
        group_a: HashSet<NodeId>,
        group_b: HashSet<NodeId>,
        start: Duration,
        duration: Option<Duration>,
    ) {
        self.partitions.push(Partition {
            group_a,
            group_b,
            start,
            end: duration.map(|d| start + d),
        });
    }
}
```

### Test Examples

#### 1. SWIM Failure Detection

```rust
#[test]
fn test_swim_failure_detection() {
    let mut sim = NetworkSimulator::new()
        .with_nodes(10)
        .with_topology(Topology::Mesh)
        .with_seed(42);

    sim.set_link_config_all(LinkConfig {
        latency_ms: 50,
        jitter_ms: 10,
        bandwidth_bps: 1_000_000,
        packet_loss_rate: 0.01,
        enabled: true,
    });

    // Run for 30 seconds to establish membership
    sim.run_for(Duration::from_secs(30)).unwrap();

    // Crash node 5
    sim.chaos.inject_failure(
        5,
        Duration::from_secs(30),
        FailureType::Crash,
    );

    // Run until detection
    sim.run_for(Duration::from_secs(10)).unwrap();

    // Verify all nodes detected failure within 5 seconds
    for node in &sim.nodes {
        if node.id != 5 {
            let detection_time = node.failure_detected_at(5);
            assert!(detection_time.is_some());
            assert!(detection_time.unwrap() < Duration::from_secs(35));
        }
    }
}
```

#### 2. Partition Recovery

```rust
#[test]
fn test_partition_recovery() {
    let mut sim = NetworkSimulator::new()
        .with_nodes(20)
        .with_topology(Topology::Mesh);

    // Split network in half
    let group_a: HashSet<_> = (0..10).collect();
    let group_b: HashSet<_> = (10..20).collect();

    sim.chaos.partition(
        group_a.clone(),
        group_b.clone(),
        Duration::from_secs(10),
        Some(Duration::from_secs(30)), // Heal after 30s
    );

    // Run through partition and healing
    sim.run_for(Duration::from_secs(60)).unwrap();

    // Verify convergence after healing
    let node_0_view = sim.nodes[0].active_view();
    let node_15_view = sim.nodes[15].active_view();

    // Cross-partition connections should be restored
    assert!(node_0_view.iter().any(|p| group_b.contains(p)));
    assert!(node_15_view.iter().any(|p| group_a.contains(p)));
}
```

#### 3. Message Propagation

```rust
#[test]
fn test_plumtree_propagation() {
    let mut sim = NetworkSimulator::new()
        .with_nodes(100)
        .with_topology(Topology::Mesh);

    // Publish message from node 0
    sim.nodes[0].publish(b"test message").unwrap();

    // Run until propagation complete
    let stats = sim.run_for(Duration::from_secs(10)).unwrap();

    // Verify all nodes received message
    for node in &sim.nodes {
        assert!(node.received_messages().contains(b"test message"));
    }

    // Check propagation efficiency
    assert!(stats.message_count < 200); // Should be O(N), not O(N^2)
    assert!(stats.max_latency < Duration::from_millis(500));
}
```

### Statistics Collection

```rust
#[derive(Debug, Default)]
pub struct SimStats {
    /// Total messages sent
    pub message_count: u64,
    /// Messages dropped (loss)
    pub messages_dropped: u64,
    /// Average delivery latency
    pub avg_latency: Duration,
    /// Maximum delivery latency
    pub max_latency: Duration,
    /// P99 delivery latency
    pub p99_latency: Duration,
    /// Bytes transferred
    pub bytes_transferred: u64,
    /// Virtual time elapsed
    pub simulation_time: Duration,
    /// Wall clock time elapsed
    pub wall_clock_time: Duration,
}

impl NetworkSimulator {
    pub fn collect_stats(&self) -> SimStats {
        // Aggregate statistics from simulation run
        // ...
    }
}
```

## Consequences

### Benefits

1. **Reproducibility**: Same seed = same execution
2. **Speed**: Hours of network time in seconds
3. **Coverage**: Test rare failure scenarios easily
4. **Debugging**: Exact event sequence available
5. **CI-friendly**: Deterministic, fast, no infrastructure

### Trade-offs

1. **Abstraction gap**: Simulated != real network behavior
2. **Implementation cost**: Custom simulator required
3. **Maintenance**: Keep simulator in sync with protocol changes
4. **False confidence**: Passing simulated tests != production ready

### Validation Strategy

Complement simulation with real network tests:

```rust
#[cfg(feature = "integration-tests")]
mod integration {
    #[tokio::test]
    async fn test_real_network_propagation() {
        // Spawn actual network nodes
        let nodes = spawn_test_network(10).await;

        // Run same test as simulation
        nodes[0].publish(b"test message").await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Verify propagation
        for node in &nodes {
            assert!(node.received_messages().await.contains(b"test message"));
        }
    }
}
```

## Alternatives Considered

### 1. Real Network Testing Only

Deploy actual nodes for all tests.

**Rejected because**:
- Slow, expensive
- Non-deterministic
- Hard to create failure scenarios

### 2. Mock Objects

Unit test with mock transports.

**Rejected because**:
- Doesn't capture distributed behavior
- Complex mock setups
- Misses timing issues

### 3. Discrete Event Simulation (SimPy-style)

Use existing DES framework.

**Rejected because**:
- Python/external dependency
- Need Rust integration
- Custom protocol awareness needed

### 4. Network Emulation (tc, netem)

Use Linux traffic control for delays/loss.

**Rejected because**:
- Non-deterministic timing
- OS-specific
- Harder to reproduce exactly

## References

- **FoundationDB's simulation**: Inspiration for deterministic testing
- **Implementation**: `crates/simulator/src/`
- **Tests using simulator**: `tests/` directory
- **Chaos scenarios**: `crates/simulator/src/chaos.rs`

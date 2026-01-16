# ADR-008: QUIC Stream Multiplexing Design

## Status

Accepted (2025-12-24)

## Context

P2P protocols generate diverse traffic patterns:

| Traffic Type | Characteristics | Priority |
|--------------|-----------------|----------|
| Membership (SWIM) | Small, frequent, latency-sensitive | High |
| PubSub control | Small/medium, bursty | High |
| Bulk data (CRDTs) | Large, can tolerate delay | Low |
| Presence beacons | Small, periodic | Medium |

Traditional approaches create problems:

| Approach | Problem |
|----------|---------|
| Single TCP connection | Head-of-line blocking |
| Multiple TCP connections | Connection explosion, NAT issues |
| UDP with custom protocol | Complex reliability, no TLS |
| Per-message connections | Connection overhead |

We needed a transport that:
- Avoids head-of-line blocking
- Uses single connection per peer pair
- Supports prioritization
- Provides built-in encryption
- Handles NAT traversal

## Decision

Use **QUIC with three multiplexed streams** per connection, via ant-quic:

### Stream Architecture

```
Per Peer Connection
┌─────────────────────────────────────────────────────┐
│                   QUIC Connection                    │
│  (single UDP flow, ML-KEM-768 handshake)            │
│                                                      │
│  ┌─────────────┬─────────────┬─────────────┐        │
│  │   Stream 0  │   Stream 1  │   Stream 2  │        │
│  │   (mship)   │   (pubsub)  │   (bulk)    │        │
│  ├─────────────┼─────────────┼─────────────┤        │
│  │ HyParView   │ Plumtree    │ CRDT deltas │        │
│  │ SWIM        │ EAGER/IHAVE │ Site blocks │        │
│  │ Shuffles    │ IWANT/PRUNE │ Large files │        │
│  │ Probes      │ Subscriptions│ Bulk sync   │        │
│  └─────────────┴─────────────┴─────────────┘        │
│                                                      │
└─────────────────────────────────────────────────────┘
```

### Stream Types

```rust
/// Transport stream types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    /// Membership protocol messages (HyParView, SWIM)
    /// Priority: High, Size: Small
    Membership,

    /// PubSub control messages (Plumtree)
    /// Priority: High, Size: Small to Medium
    PubSub,

    /// Bulk data transfer (CRDTs, large payloads)
    /// Priority: Low, Size: Large
    Bulk,
}

impl StreamType {
    /// QUIC stream ID for this type
    pub fn stream_id(&self) -> u64 {
        match self {
            StreamType::Membership => 0,
            StreamType::PubSub => 1,
            StreamType::Bulk => 2,
        }
    }

    /// Priority for congestion control
    pub fn priority(&self) -> u8 {
        match self {
            StreamType::Membership => 255, // Highest
            StreamType::PubSub => 200,
            StreamType::Bulk => 50,
        }
    }
}
```

### Transport Trait

```rust
#[async_trait]
pub trait GossipTransport: Send + Sync + 'static {
    /// Connect to a peer at known address
    async fn dial(&self, peer: PeerId, addr: SocketAddr) -> Result<()>;

    /// Connect to bootstrap peer (returns their PeerId)
    async fn dial_bootstrap(&self, addr: SocketAddr) -> Result<PeerId>;

    /// Start listening for connections
    async fn listen(&self, bind: SocketAddr) -> Result<()>;

    /// Close the transport
    async fn close(&self) -> Result<()>;

    /// Send message on specified stream
    async fn send_to_peer(
        &self,
        peer: PeerId,
        stream_type: StreamType,
        data: Bytes,
    ) -> Result<()>;

    /// Receive next message
    async fn receive_message(&self) -> Result<(PeerId, StreamType, Bytes)>;
}
```

### QUIC Benefits

#### 1. No Head-of-Line Blocking

```
TCP (single connection):
  [CRDT block 1][SWIM probe][CRDT block 2]
  If block 1 is lost, probe is delayed until retransmit

QUIC (multiplexed streams):
  Stream 0: ────[SWIM probe]────────────────→ (delivered immediately)
  Stream 2: ────[CRDT block 1]──LOSS──RETX──→ (delayed independently)

Result: High-priority traffic not blocked by bulk data loss
```

#### 2. Connection Migration

```rust
// When network path changes (WiFi → cellular)
impl AntQuicTransport {
    fn on_path_change(&mut self, new_addr: SocketAddr) {
        // QUIC connection IDs allow migration without reconnection
        self.connection.migrate_to(new_addr);
        // No handshake needed, in-flight data preserved
    }
}
```

#### 3. 0-RTT Resumption

```rust
// Reconnecting to known peer
impl AntQuicTransport {
    async fn reconnect(&mut self, peer: PeerId) -> Result<()> {
        if let Some(session_ticket) = self.session_cache.get(&peer) {
            // 0-RTT: Send data immediately with resumption
            self.connect_with_ticket(peer, session_ticket).await
        } else {
            // Full handshake (still 1-RTT)
            self.connect_fresh(peer).await
        }
    }
}
```

### Message Framing

Messages are framed on each stream:

```rust
/// Wire format for stream messages
struct WireMessage {
    /// Message length (4 bytes, big-endian)
    length: u32,
    /// Message type tag (1 byte)
    msg_type: u8,
    /// Payload (length - 1 bytes)
    payload: Vec<u8>,
}

impl WireMessage {
    async fn read(stream: &mut QuicStream) -> Result<Self> {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let length = u32::from_be_bytes(len_buf);

        let mut data = vec![0u8; length as usize];
        stream.read_exact(&mut data).await?;

        Ok(Self {
            length,
            msg_type: data[0],
            payload: data[1..].to_vec(),
        })
    }

    async fn write(&self, stream: &mut QuicStream) -> Result<()> {
        stream.write_all(&self.length.to_be_bytes()).await?;
        stream.write_all(&[self.msg_type]).await?;
        stream.write_all(&self.payload).await?;
        Ok(())
    }
}
```

### Congestion Control

QUIC's congestion control applies per-connection, with stream priorities:

```rust
impl AntQuicTransport {
    fn configure_priorities(&self) {
        // Membership stream: never throttled
        self.connection.set_stream_priority(0, StreamPriority::Highest);

        // PubSub: high priority
        self.connection.set_stream_priority(1, StreamPriority::High);

        // Bulk: best-effort, yields to above
        self.connection.set_stream_priority(2, StreamPriority::Low);
    }
}
```

### Connection Lifecycle

```rust
impl AntQuicTransport {
    /// Full connection establishment flow
    async fn establish_connection(&mut self, peer: PeerId, addr: SocketAddr) -> Result<()> {
        // 1. QUIC handshake (ML-KEM-768 key exchange, ML-DSA-65 cert)
        let conn = self.endpoint.connect(addr).await?;

        // 2. Verify peer identity
        let peer_cert = conn.peer_certificate()?;
        let actual_peer_id = PeerId::from_pubkey(&peer_cert.public_key);
        if actual_peer_id != peer {
            return Err(Error::PeerIdMismatch);
        }

        // 3. Open streams
        let mship_stream = conn.open_bi().await?;
        let pubsub_stream = conn.open_bi().await?;
        let bulk_stream = conn.open_bi().await?;

        // 4. Store connection
        self.connections.insert(peer, Connection {
            quic: conn,
            streams: [mship_stream, pubsub_stream, bulk_stream],
        });

        Ok(())
    }
}
```

## Consequences

### Benefits

1. **No head-of-line blocking**: Streams are independent
2. **Single connection**: One UDP flow per peer pair
3. **Built-in encryption**: QUIC TLS 1.3 with PQC
4. **NAT traversal**: UDP hole punching, connection migration
5. **Priority support**: High-priority traffic not delayed
6. **0-RTT resumption**: Fast reconnection to known peers

### Trade-offs

1. **UDP dependency**: Some networks block UDP
2. **Complexity**: QUIC is more complex than TCP
3. **Library maturity**: Fewer QUIC implementations than TCP

### Fallback Strategy

If QUIC is blocked:

```rust
impl TransportStack {
    async fn connect(&mut self, peer: PeerId, addr: SocketAddr) -> Result<()> {
        // Try QUIC first
        if let Ok(()) = self.quic.dial(peer, addr).await {
            return Ok(());
        }

        // Fall back to relay if available
        if let Some(relay) = self.find_relay().await {
            return self.relay_connect(peer, relay).await;
        }

        Err(Error::ConnectionFailed)
    }
}
```

## Alternatives Considered

### 1. TCP with Application Multiplexing

Use single TCP connection with framing.

**Rejected because**:
- Head-of-line blocking inherent to TCP
- Custom multiplexing adds complexity
- No built-in encryption

### 2. Multiple TCP Connections

Open separate connection per stream type.

**Rejected because**:
- Connection explosion (3x connections)
- NAT state exhaustion
- More complex connection management

### 3. WebRTC Data Channels

Use WebRTC for P2P connectivity.

**Rejected because**:
- Primarily designed for browsers
- More complex than needed
- Large dependency footprint

### 4. Raw UDP with Custom Protocol

Implement reliability on UDP.

**Rejected because**:
- Reinventing QUIC
- Security analysis expensive
- Congestion control complex

### 5. libp2p Transports

Use libp2p's transport abstraction.

**Rejected because**:
- Additional dependency
- Less control over QUIC configuration
- Our transport needs are simpler

## References

- **QUIC**: RFC 9000 (QUIC: A UDP-Based Multiplexed and Secure Transport)
- **ant-quic**: Transport layer implementation (see ant-quic ADRs)
- **Implementation**: `crates/transport/src/`
- **Stream types**: `crates/transport/src/stream_type.rs`

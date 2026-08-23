//! Transport layer error types.
//!
//! This module provides error types for transport operations using thiserror
//! for ergonomic error handling.

use std::net::SocketAddr;

use saorsa_gossip_types::PeerId;
use thiserror::Error;

/// Result type alias for transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Errors that can occur during transport operations.
#[derive(Debug, Error)]
pub enum TransportError {
    /// Failed to establish a connection to a peer.
    #[error("Connection failed to peer {peer_id:?} at {addr}: {source}")]
    ConnectionFailed {
        /// The peer ID if known.
        peer_id: Option<PeerId>,
        /// The socket address that failed.
        addr: SocketAddr,
        /// The underlying error.
        #[source]
        source: anyhow::Error,
    },

    /// Failed to send data to a peer.
    #[error("Send failed to peer {peer_id}: {source}")]
    SendFailed {
        /// The peer ID.
        peer_id: PeerId,
        /// The underlying error.
        #[source]
        source: anyhow::Error,
    },

    /// Failed to receive data from the transport.
    #[error("Receive failed: {source}")]
    ReceiveFailed {
        /// The underlying error.
        #[source]
        source: anyhow::Error,
    },

    /// Failed to dial a remote address.
    #[error("Dial failed to {addr}: {source}")]
    DialFailed {
        /// The socket address that failed.
        addr: SocketAddr,
        /// The underlying error.
        #[source]
        source: anyhow::Error,
    },

    /// Invalid peer ID.
    #[error("Invalid peer ID: {reason}")]
    InvalidPeerId {
        /// The reason for invalidity.
        reason: String,
    },

    /// Invalid configuration.
    #[error("Invalid configuration: {reason}")]
    InvalidConfig {
        /// The reason for invalidity.
        reason: String,
    },

    /// Message exceeds transport MTU.
    #[error("Message size {size} exceeds MTU {mtu}")]
    MtuExceeded {
        /// The actual message size.
        size: usize,
        /// The maximum allowed size.
        mtu: usize,
    },

    /// Transport is closed.
    #[error("Transport is closed")]
    Closed,

    /// Operation timed out.
    #[error("Operation '{operation}' timed out")]
    Timeout {
        /// The operation that timed out.
        operation: String,
    },

    /// Other transport error.
    #[error("Transport error: {source}")]
    Other {
        /// The underlying error.
        #[source]
        source: anyhow::Error,
    },

    /// The peer has no live transport connection.
    ///
    /// Definitive and instant — e.g. ant-quic's `EndpointError::PeerNotFound`
    /// after a connection-generation replacement — as opposed to a timeout on
    /// a live connection. PubSub uses this classification to evict the peer
    /// from its topic sets instead of feeding per-peer timeout accounting and
    /// cooling (x0x #380): the periodic `set_topic_peers` refresh re-adds the
    /// peer as soon as the transport reports it connected again.
    #[error("Peer {peer_id} is not connected")]
    PeerNotConnected {
        /// The peer that has no live connection.
        peer_id: PeerId,
    },
}

/// Display substrings that identify a definitive "not connected" failure in
/// error text produced by transports that wrap their underlying endpoint
/// error as a string (x0x's ant-quic binding surfaces
/// `… Endpoint error: Peer not found: PeerId(…)` inside `SendFailed` /
/// `Other`). Kept as a documented fallback so classification works before
/// those bindings migrate to [`TransportError::PeerNotConnected`] (x0x #380).
const NOT_CONNECTED_SENTINELS: [&str; 3] = ["peer not found", "not connected", "no cached address"];

impl TransportError {
    /// Whether this error definitively reports that the target peer has no
    /// live transport connection.
    ///
    /// Primary signal: the [`TransportError::PeerNotConnected`] variant.
    /// Fallback: sentinel substrings over the rendered error — thiserror's
    /// `#[error("… {source}")]` folds the whole source chain into the
    /// message, so one `contains` covers wrapped endpoint errors.
    ///
    /// x0x #380: an unconnected peer is not a slow peer. An instant
    /// `PeerNotFound` must evict the peer from PubSub topic sets; booking it
    /// as a timeout fed cooling, collapsed the eager mesh, and sustained the
    /// GRAFT/PRUNE storm under connection churn.
    pub fn is_peer_not_connected(&self) -> bool {
        if matches!(self, TransportError::PeerNotConnected { .. }) {
            return true;
        }
        let rendered = self.to_string().to_lowercase();
        NOT_CONNECTED_SENTINELS.iter().any(|s| rendered.contains(s))
    }
}

/// Classify an `anyhow::Error` returned by [`GossipTransport::send_to_peer`]
/// as a definitive peer-not-connected failure.
///
/// Same contract as [`TransportError::is_peer_not_connected`]: a structured
/// `TransportError::PeerNotConnected` when the transport emits one, else the
/// documented sentinel-text fallback over the full anyhow chain (`{:#}`
/// renders every cause).
pub fn is_peer_not_connected_error(err: &anyhow::Error) -> bool {
    if let Some(transport_error) = err.downcast_ref::<TransportError>() {
        return transport_error.is_peer_not_connected();
    }
    let rendered = format!("{err:#}").to_lowercase();
    NOT_CONNECTED_SENTINELS.iter().any(|s| rendered.contains(s))
}

impl From<anyhow::Error> for TransportError {
    fn from(source: anyhow::Error) -> Self {
        TransportError::Other { source }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(err: std::io::Error) -> Self {
        TransportError::Other { source: err.into() }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
mod not_connected_classification_tests {
    use super::*;
    use anyhow::anyhow;

    /// x0x #380: classification is load-bearing — a false positive cools
    /// nothing (harmless), but a false negative re-opens the cooling storm.
    /// Pin both directions for the variant, the sentinel fallback, and
    /// non-sentinel errors.
    #[test]
    fn variant_classifies_as_not_connected() {
        let err = TransportError::PeerNotConnected {
            peer_id: PeerId::new([1; 32]),
        };
        assert!(err.is_peer_not_connected());
    }

    #[test]
    fn ant_quic_peer_not_found_text_classifies_via_fallback() {
        // The shape x0x's transport binding produces today: ant-quic's
        // EndpointError wrapped into SendFailed's anyhow source, folded
        // into the rendered message by thiserror.
        let err = TransportError::SendFailed {
            peer_id: PeerId::new([2; 32]),
            source: anyhow!("send failed: Endpoint error: Peer not found: PeerId([9, 9])"),
        };
        assert!(err.is_peer_not_connected());
    }

    #[test]
    fn anyhow_wrapped_not_connected_text_classifies_via_fallback() {
        let err: anyhow::Error = TransportError::SendFailed {
            peer_id: PeerId::new([3; 32]),
            source: anyhow!("Endpoint error: Peer not found: PeerId([9, 9])"),
        }
        .into();
        assert!(is_peer_not_connected_error(&err));
    }

    #[test]
    fn adapter_not_connected_text_classifies_via_fallback() {
        // sg's own pre-#380 wording for the no-address path.
        let err = TransportError::Other {
            source: anyhow!("Peer ab12 not connected and no cached address is available"),
        };
        assert!(err.is_peer_not_connected());
    }

    #[test]
    fn live_connection_errors_do_not_classify() {
        let live = TransportError::SendFailed {
            peer_id: PeerId::new([4; 32]),
            source: anyhow!("send failed: connection reset by peer"),
        };
        assert!(!live.is_peer_not_connected());

        let timeout = TransportError::Timeout {
            operation: "send_to_peer".to_string(),
        };
        assert!(!timeout.is_peer_not_connected());

        let anyhow_live: anyhow::Error = anyhow!("send failed: connection reset by peer");
        assert!(!is_peer_not_connected_error(&anyhow_live));
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_connection_failed_error() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let peer_id = PeerId::new([1u8; 32]);
        let err = TransportError::ConnectionFailed {
            peer_id: Some(peer_id),
            addr,
            source: anyhow::anyhow!("connection refused"),
        };

        let msg = err.to_string();
        assert!(msg.contains("Connection failed"));
        assert!(msg.contains("127.0.0.1:8080"));
    }

    #[test]
    fn test_send_failed_error() {
        let peer_id = PeerId::new([2u8; 32]);
        let err = TransportError::SendFailed {
            peer_id,
            source: anyhow::anyhow!("send buffer full"),
        };

        let msg = err.to_string();
        assert!(msg.contains("Send failed"));
    }

    #[test]
    fn test_receive_failed_error() {
        let err = TransportError::ReceiveFailed {
            source: anyhow::anyhow!("connection reset"),
        };

        let msg = err.to_string();
        assert!(msg.contains("Receive failed"));
    }

    #[test]
    fn test_dial_failed_error() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 9000);
        let err = TransportError::DialFailed {
            addr,
            source: anyhow::anyhow!("network unreachable"),
        };

        let msg = err.to_string();
        assert!(msg.contains("Dial failed"));
        assert!(msg.contains("192.168.1.1:9000"));
    }

    #[test]
    fn test_invalid_peer_id_error() {
        let err = TransportError::InvalidPeerId {
            reason: "malformed peer ID".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("Invalid peer ID"));
        assert!(msg.contains("malformed peer ID"));
    }

    #[test]
    fn test_invalid_config_error() {
        let err = TransportError::InvalidConfig {
            reason: "missing required field".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("Invalid configuration"));
        assert!(msg.contains("missing required field"));
    }

    #[test]
    fn test_closed_error() {
        let err = TransportError::Closed;
        let msg = err.to_string();
        assert!(msg.contains("Transport is closed"));
    }

    #[test]
    fn test_timeout_error() {
        let err = TransportError::Timeout {
            operation: "connect".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("timed out"));
        assert!(msg.contains("connect"));
    }

    #[test]
    fn test_other_error() {
        let err = TransportError::Other {
            source: anyhow::anyhow!("unknown error"),
        };

        let msg = err.to_string();
        assert!(msg.contains("Transport error"));
    }

    #[test]
    fn test_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("test error");
        let transport_err: TransportError = anyhow_err.into();

        match transport_err {
            TransportError::Other { .. } => {}
            _ => panic!("Expected TransportError::Other"),
        }
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let transport_err: TransportError = io_err.into();

        match transport_err {
            TransportError::Other { .. } => {}
            _ => panic!("Expected TransportError::Other"),
        }
    }
}

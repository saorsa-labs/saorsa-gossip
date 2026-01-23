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

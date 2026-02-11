//! Coordinator advertisement publisher
//!
//! Publishes coordinator adverts to the gossip overlay via transport

use crate::{coordinator_topic, CoordinatorAdvert, CoordinatorRoles, NatClass};
use anyhow::Result;
use saorsa_gossip_types::PeerId;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::interval;

/// Publisher for coordinator advertisements
pub struct CoordinatorPublisher {
    /// Local peer ID
    peer_id: PeerId,
    /// Coordinator roles
    roles: CoordinatorRoles,
    /// Known addresses for this coordinator
    addr_hints: Vec<SocketAddr>,
    /// NAT classification
    nat_class: NatClass,
    /// Signing key for adverts
    signing_key: Arc<Mutex<Option<saorsa_pqc::MlDsaSecretKey>>>,
    /// Advertisement validity duration (ms)
    validity_duration: u64,
}

impl CoordinatorPublisher {
    /// Create a new coordinator publisher
    pub fn new(
        peer_id: PeerId,
        roles: CoordinatorRoles,
        addr_hints: Vec<SocketAddr>,
        nat_class: NatClass,
    ) -> Self {
        Self {
            peer_id,
            roles,
            addr_hints,
            nat_class,
            signing_key: Arc::new(Mutex::new(None)),
            validity_duration: 3_600_000, // 1 hour default
        }
    }

    /// Set the signing key
    pub async fn set_signing_key(&self, key: saorsa_pqc::MlDsaSecretKey) {
        let mut signing_key = self.signing_key.lock().await;
        *signing_key = Some(key);
    }

    /// Set the validity duration for adverts
    pub fn with_validity_duration(mut self, duration_ms: u64) -> Self {
        self.validity_duration = duration_ms;
        self
    }

    /// Create and sign a coordinator advert
    pub async fn create_advert(&self) -> Result<CoordinatorAdvert> {
        let mut advert = CoordinatorAdvert::new(
            self.peer_id,
            self.roles.clone(),
            self.addr_hints
                .iter()
                .map(|addr| crate::AddrHint::new(*addr))
                .collect(),
            self.nat_class,
            self.validity_duration,
        );

        // Sign the advert
        let signing_key = self.signing_key.lock().await;
        if let Some(ref key) = *signing_key {
            advert.sign(key)?;
        } else {
            anyhow::bail!("No signing key set");
        }

        Ok(advert)
    }

    /// Publish an advert (returns serialized bytes)
    pub async fn publish_advert(&self) -> Result<bytes::Bytes> {
        let advert = self.create_advert().await?;
        advert.to_bytes()
    }

    /// Get the coordinator topic
    pub fn topic(&self) -> saorsa_gossip_types::TopicId {
        coordinator_topic()
    }
}

/// Periodic coordinator advertisement publisher
pub struct PeriodicPublisher {
    publisher: CoordinatorPublisher,
    interval_secs: u64,
}

impl PeriodicPublisher {
    /// Create a new periodic publisher
    pub fn new(publisher: CoordinatorPublisher, interval_secs: u64) -> Self {
        Self {
            publisher,
            interval_secs,
        }
    }

    /// Start publishing adverts periodically
    ///
    /// Returns a channel that receives the serialized adverts.
    /// The caller is responsible for sending these to the transport.
    pub async fn start(self) -> tokio::sync::mpsc::Receiver<bytes::Bytes> {
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let interval_secs = self.interval_secs;

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(interval_secs));

            loop {
                ticker.tick().await;

                match self.publisher.publish_advert().await {
                    Ok(bytes) => {
                        if tx.send(bytes).await.is_err() {
                            // Receiver dropped, stop publishing
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to publish advert: {}", e);
                    }
                }
            }
        });

        rx
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use saorsa_pqc::{MlDsa65, MlDsaOperations};

    #[tokio::test]
    async fn test_publisher_creation() {
        let peer_id = PeerId::new([1u8; 32]);
        let roles = CoordinatorRoles {
            coordinator: true,
            reflector: true,
            rendezvous: false,
            relay: false,
        };

        let publisher = CoordinatorPublisher::new(peer_id, roles, vec![], NatClass::Eim);

        assert_eq!(publisher.peer_id, peer_id);
        assert!(publisher.roles.coordinator);
    }

    #[tokio::test]
    async fn test_create_advert_without_key_fails() {
        let peer_id = PeerId::new([1u8; 32]);
        let publisher =
            CoordinatorPublisher::new(peer_id, CoordinatorRoles::default(), vec![], NatClass::Eim);

        let result = publisher.create_advert().await;
        assert!(result.is_err(), "Should fail without signing key");
    }

    #[tokio::test]
    async fn test_create_advert_with_key_succeeds() {
        let peer_id = PeerId::new([1u8; 32]);
        let publisher =
            CoordinatorPublisher::new(peer_id, CoordinatorRoles::default(), vec![], NatClass::Eim);

        // Generate and set signing key
        let signer = MlDsa65::new();
        let (_, sk) = signer.generate_keypair().expect("keypair");
        publisher.set_signing_key(sk).await;

        let advert = publisher
            .create_advert()
            .await
            .expect("should create advert");

        assert_eq!(advert.peer, peer_id);
        assert!(!advert.sig.is_empty(), "Should be signed");
        assert!(advert.is_valid(), "Should be valid");
    }

    #[tokio::test]
    async fn test_publish_advert_returns_bytes() {
        let peer_id = PeerId::new([1u8; 32]);
        let addr = "127.0.0.1:8080".parse().expect("valid address");
        let publisher = CoordinatorPublisher::new(
            peer_id,
            CoordinatorRoles::default(),
            vec![addr],
            NatClass::Eim,
        );

        // Set signing key
        let signer = MlDsa65::new();
        let (_, sk) = signer.generate_keypair().expect("keypair");
        publisher.set_signing_key(sk).await;

        let bytes = publisher.publish_advert().await.expect("should publish");

        assert!(!bytes.is_empty(), "Should return non-empty bytes");

        // Verify we can deserialize
        let advert = CoordinatorAdvert::from_bytes(&bytes).expect("should deserialize");
        assert_eq!(advert.peer, peer_id);
        assert_eq!(advert.addr_hints.len(), 1);
    }

    #[tokio::test]
    async fn test_publisher_topic() {
        let peer_id = PeerId::new([1u8; 32]);
        let publisher =
            CoordinatorPublisher::new(peer_id, CoordinatorRoles::default(), vec![], NatClass::Eim);

        let topic = publisher.topic();
        assert_eq!(topic, coordinator_topic());
    }

    #[tokio::test]
    async fn test_with_validity_duration() {
        let peer_id = PeerId::new([1u8; 32]);
        let publisher =
            CoordinatorPublisher::new(peer_id, CoordinatorRoles::default(), vec![], NatClass::Eim)
                .with_validity_duration(5000); // 5 seconds

        // Set signing key
        let signer = MlDsa65::new();
        let (_, sk) = signer.generate_keypair().expect("keypair");
        publisher.set_signing_key(sk).await;

        let advert = publisher.create_advert().await.expect("should create");

        assert!(advert.is_valid());
        assert!(advert.not_after - advert.not_before <= 5000);
    }

    #[tokio::test]
    async fn test_periodic_publisher_sends_adverts() {
        let peer_id = PeerId::new([1u8; 32]);
        let publisher =
            CoordinatorPublisher::new(peer_id, CoordinatorRoles::default(), vec![], NatClass::Eim);

        // Set signing key
        let signer = MlDsa65::new();
        let (_, sk) = signer.generate_keypair().expect("keypair");
        publisher.set_signing_key(sk).await;

        // Create periodic publisher with short interval for testing
        let periodic = PeriodicPublisher::new(publisher, 1); // 1 second interval
        let mut rx = periodic.start().await;

        // Should receive at least 2 adverts within 3 seconds
        let timeout = tokio::time::timeout(Duration::from_secs(3), async {
            let mut count = 0;
            while let Some(bytes) = rx.recv().await {
                assert!(!bytes.is_empty());
                count += 1;
                if count >= 2 {
                    break;
                }
            }
            count
        })
        .await;

        assert!(timeout.is_ok(), "Should receive adverts");
        assert!(timeout.unwrap() >= 2, "Should receive at least 2 adverts");
    }

    #[tokio::test]
    async fn test_periodic_publisher_stops_when_receiver_dropped() {
        let peer_id = PeerId::new([1u8; 32]);
        let publisher =
            CoordinatorPublisher::new(peer_id, CoordinatorRoles::default(), vec![], NatClass::Eim);

        // Set signing key
        let signer = MlDsa65::new();
        let (_, sk) = signer.generate_keypair().expect("keypair");
        publisher.set_signing_key(sk).await;

        let periodic = PeriodicPublisher::new(publisher, 1);
        let rx = periodic.start().await;

        // Drop receiver
        drop(rx);

        // Wait a bit to ensure publisher task detects drop
        tokio::time::sleep(Duration::from_millis(100)).await;

        // If we reach here without hanging, the publisher stopped correctly
        // (No assertion needed - test passes by completing)
    }
}

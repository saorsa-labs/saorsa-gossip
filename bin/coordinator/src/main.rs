//! Saorsa Gossip Coordinator Node
//!
//! This binary runs a bootstrap/coordinator node for the Saorsa Gossip network.
//! Per SPEC2 §8, coordinators provide:
//! - Bootstrap discovery
//! - Address reflection for NAT traversal
//! - Optional relay services
//! - Optional rendezvous services
//!
//! # Usage
//!
//! ```bash
//! coordinator --bind 0.0.0.0:7000 --role coordinator,reflector,relay
//! ```

use anyhow::Result;
use bytes::Bytes;
use clap::Parser;
use saorsa_gossip_transport::{GossipStreamType, GossipTransport};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

mod updater;

/// Saorsa Gossip Coordinator Node
#[derive(Parser, Debug)]
#[command(name = "saorsa-coordinator")]
#[command(about = "Saorsa Gossip Network Coordinator Node", long_about = None)]
struct Args {
    /// Address to bind to (e.g., 0.0.0.0:7000)
    #[arg(short, long, default_value = "0.0.0.0:7000")]
    bind: SocketAddr,

    /// Coordinator roles (comma-separated): coordinator,reflector,relay,rendezvous
    #[arg(short, long, default_value = "coordinator,reflector")]
    roles: String,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Identity file path (ML-DSA keypair)
    #[arg(long, default_value = "~/.saorsa-gossip/coordinator.identity")]
    identity_path: PathBuf,

    /// Publish interval in seconds (coordinator adverts)
    #[arg(long, default_value = "300")]
    publish_interval: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    init_logging(args.verbose)?;

    tracing::info!("Starting Saorsa Gossip Coordinator");
    tracing::info!("Bind address: {}", args.bind);
    tracing::info!("Roles: {}", args.roles);

    // Parse roles
    let coordinator_roles = parse_roles(&args.roles)?;
    tracing::info!("Parsed roles: {:?}", coordinator_roles);

    // 1. Load or create identity
    let identity = load_or_create_identity(&args.identity_path).await?;
    tracing::info!(
        "Loaded identity: {}",
        hex::encode(identity.peer_id.as_bytes())
    );

    // 2. Start coordinator services based on roles
    let mut advert_rx = if coordinator_roles.coordinator {
        tracing::info!("Starting coordinator advertisement service...");
        Some(
            start_coordinator_service(
                &identity,
                &coordinator_roles,
                args.bind,
                args.publish_interval,
            )
            .await?,
        )
    } else {
        None
    };

    // 3. Start transport and message handling
    tracing::info!("Initializing transport on {}...", args.bind);
    let transport = saorsa_gossip_transport::UdpTransportAdapter::new(
        args.bind,
        vec![], // Coordinator nodes don't need other known peers
    )
    .await?;

    tracing::info!(
        "Transport initialized - PeerId: {}",
        hex::encode(transport.peer_id().as_bytes())
    );

    if coordinator_roles.reflector {
        tracing::info!("Reflector role enabled (address observation)");
        // Address reflection is handled by responding to PING messages
    }

    if coordinator_roles.relay {
        tracing::info!("Relay role enabled (message forwarding)");
        // TODO: Implement relay service with rate limiting
    }

    if coordinator_roles.rendezvous {
        tracing::info!("Rendezvous role enabled (connection coordination)");
        // TODO: Implement rendezvous coordination
    }

    tracing::info!("Coordinator node running. Press Ctrl+C to stop.");

    // 4. Start background update checker
    tracing::info!("Starting background update checker (checks every 6 hours)...");
    updater::start_background_checker();

    // 5. Start message handling loop
    let transport = std::sync::Arc::new(transport);
    let publish_interval = args.publish_interval;
    if let Some(mut advert_stream) = advert_rx.take() {
        let transport_for_adverts = transport.clone();
        tokio::spawn(async move {
            tracing::info!(
                "Coordinator advert publisher started (interval: {}s)",
                publish_interval
            );

            while let Some(advert) = advert_stream.recv().await {
                let peers = transport_for_adverts.connected_peers().await;
                if peers.is_empty() {
                    tracing::debug!("No connected peers to send coordinator adverts");
                    continue;
                }

                for (peer_id, _) in peers {
                    if let Err(e) = transport_for_adverts
                        .send_to_peer(peer_id, GossipStreamType::PubSub, advert.clone())
                        .await
                    {
                        tracing::warn!(?peer_id, ?e, "Failed to send coordinator advert");
                    } else {
                        tracing::trace!(?peer_id, "Sent coordinator advert");
                    }
                }
            }

            tracing::info!("Coordinator advert publisher stopped");
        });
    }

    let transport_clone = transport.clone();

    tokio::spawn(async move {
        handle_messages(transport_clone).await;
    });

    // 6. Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down coordinator...");

    Ok(())
}

/// Load or create a coordinator identity
async fn load_or_create_identity(path: &Path) -> Result<CoordinatorIdentity> {
    // Expand tilde in path
    let expanded_path = shellexpand::tilde(&path.to_string_lossy()).to_string();
    let identity_path = PathBuf::from(expanded_path);

    // Ensure parent directory exists
    if let Some(parent) = identity_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Try to load existing identity
    if identity_path.exists() {
        tracing::info!("Loading identity from: {}", identity_path.display());
        let identity_data = tokio::fs::read(&identity_path).await?;
        let keypair = saorsa_gossip_identity::MlDsaKeyPair::from_bytes(&identity_data)?;
        let peer_id = saorsa_gossip_types::PeerId::from_pubkey(keypair.public_key());

        Ok(CoordinatorIdentity { peer_id, keypair })
    } else {
        tracing::info!("Creating new identity at: {}", identity_path.display());
        let keypair = saorsa_gossip_identity::MlDsaKeyPair::generate()?;
        let peer_id = saorsa_gossip_types::PeerId::from_pubkey(keypair.public_key());

        // Save to disk
        let identity_data = keypair.to_bytes()?;
        tokio::fs::write(&identity_path, &identity_data).await?;

        tracing::info!("Identity saved to: {}", identity_path.display());
        Ok(CoordinatorIdentity { peer_id, keypair })
    }
}

/// Start the coordinator advertisement service
async fn start_coordinator_service(
    identity: &CoordinatorIdentity,
    roles: &CoordinatorRoles,
    bind_addr: SocketAddr,
    publish_interval_secs: u64,
) -> Result<tokio::sync::mpsc::Receiver<Bytes>> {
    use saorsa_gossip_coordinator::{CoordinatorPublisher, NatClass, PeriodicPublisher};

    // Create publisher
    let publisher = CoordinatorPublisher::new(
        identity.peer_id,
        roles.clone().into(),
        vec![bind_addr],
        NatClass::Eim, // Default to EIM, can be detected via STUN
    );

    // Set signing key
    let secret_key = identity.keypair.get_secret_key_typed()?;
    publisher.set_signing_key(secret_key).await;

    // Start periodic publishing
    let periodic = PeriodicPublisher::new(publisher, publish_interval_secs);
    Ok(periodic.start().await)
}

/// Coordinator identity wrapper
struct CoordinatorIdentity {
    peer_id: saorsa_gossip_types::PeerId,
    keypair: saorsa_gossip_identity::MlDsaKeyPair,
}

/// Parse coordinator roles from comma-separated string
fn parse_roles(roles_str: &str) -> Result<CoordinatorRoles> {
    let mut roles = CoordinatorRoles::default();

    for role in roles_str.split(',').map(|s| s.trim()) {
        match role.to_lowercase().as_str() {
            "coordinator" => roles.coordinator = true,
            "reflector" => roles.reflector = true,
            "relay" => roles.relay = true,
            "rendezvous" => roles.rendezvous = true,
            "" => {} // Skip empty
            unknown => {
                return Err(anyhow::anyhow!("Unknown role: {}", unknown));
            }
        }
    }

    Ok(roles)
}

/// Coordinator role flags
#[derive(Debug, Default, Clone)]
struct CoordinatorRoles {
    coordinator: bool,
    reflector: bool,
    relay: bool,
    rendezvous: bool,
}

impl From<CoordinatorRoles> for saorsa_gossip_coordinator::CoordinatorRoles {
    fn from(roles: CoordinatorRoles) -> Self {
        Self {
            coordinator: roles.coordinator,
            reflector: roles.reflector,
            rendezvous: roles.rendezvous,
            relay: roles.relay,
        }
    }
}

/// Handle incoming messages from peers
async fn handle_messages(transport: std::sync::Arc<saorsa_gossip_transport::UdpTransportAdapter>) {
    tracing::info!("Message handler started - listening for PING messages...");

    loop {
        match transport.receive_message().await {
            Ok((peer_id, stream_type, data)) => {
                tracing::debug!(
                    "Received message from peer {} on {:?} stream ({} bytes)",
                    hex::encode(peer_id.as_bytes()),
                    stream_type,
                    data.len()
                );

                // Check if this is a PING message
                if data.as_ref() == b"PING" {
                    tracing::info!(
                        "📡 PING received from peer {}",
                        hex::encode(peer_id.as_bytes())
                    );

                    // Send PONG response
                    let pong_data = bytes::Bytes::from_static(b"PONG");
                    match transport
                        .send_to_peer(peer_id, stream_type, pong_data)
                        .await
                    {
                        Ok(_) => {
                            tracing::info!(
                                "✓ PONG sent to peer {}",
                                hex::encode(peer_id.as_bytes())
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                "❌ Failed to send PONG to peer {}: {}",
                                hex::encode(peer_id.as_bytes()),
                                e
                            );
                        }
                    }
                } else {
                    tracing::debug!(
                        "Received non-PING message: {}",
                        String::from_utf8_lossy(&data)
                    );
                }
            }
            Err(e) => {
                tracing::error!("Error receiving message: {}", e);
                // Continue listening even on errors
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Initialize logging based on verbosity
fn init_logging(verbose: bool) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    let filter = if verbose {
        EnvFilter::new("trace")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // TDD RED: These tests will fail initially

    #[test]
    fn test_parse_roles_coordinator_only() {
        let roles = parse_roles("coordinator").expect("should parse");
        assert!(roles.coordinator);
        assert!(!roles.reflector);
        assert!(!roles.relay);
        assert!(!roles.rendezvous);
    }

    #[test]
    fn test_parse_roles_multiple() {
        let roles = parse_roles("coordinator,reflector,relay").expect("should parse");
        assert!(roles.coordinator);
        assert!(roles.reflector);
        assert!(roles.relay);
        assert!(!roles.rendezvous);
    }

    #[test]
    fn test_parse_roles_all() {
        let roles = parse_roles("coordinator,reflector,relay,rendezvous").expect("should parse");
        assert!(roles.coordinator);
        assert!(roles.reflector);
        assert!(roles.relay);
        assert!(roles.rendezvous);
    }

    #[test]
    fn test_parse_roles_case_insensitive() {
        let roles = parse_roles("COORDINATOR,Reflector,RELAY").expect("should parse");
        assert!(roles.coordinator);
        assert!(roles.reflector);
        assert!(roles.relay);
    }

    #[test]
    fn test_parse_roles_with_spaces() {
        let roles = parse_roles("coordinator, reflector , relay").expect("should parse");
        assert!(roles.coordinator);
        assert!(roles.reflector);
        assert!(roles.relay);
    }

    #[test]
    fn test_parse_roles_unknown_fails() {
        let result = parse_roles("coordinator,unknown");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown role"));
    }

    #[test]
    fn test_parse_roles_empty_string() {
        let roles = parse_roles("").expect("should parse empty");
        assert!(!roles.coordinator);
        assert!(!roles.reflector);
        assert!(!roles.relay);
        assert!(!roles.rendezvous);
    }
}

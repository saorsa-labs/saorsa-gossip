//! Saorsa Gossip CLI Tool
//!
//! Interactive demonstration and testing tool for the Saorsa Gossip network.
//! This CLI exercises all library features for validation and demos.
//!
//! # Commands
//!
//! - `identity` - Create and manage ML-DSA identities
//! - `network` - Join network and participate in gossip
//! - `pubsub` - Publish/subscribe to topics
//! - `presence` - Manage presence beacons
//! - `groups` - Create and join groups
//! - `crdt` - Demonstrate CRDT operations
//! - `rendezvous` - Test rendezvous coordination
//!
//! # Usage
//!
//! ```bash
//! saorsa-gossip identity create --alias "Alice"
//! saorsa-gossip network join --coordinator 127.0.0.1:7000
//! saorsa-gossip pubsub publish --topic news --message "Hello World"
//! ```

use anyhow::{anyhow, Result};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use saorsa_gossip_coordinator::CoordinatorAdvert;
use saorsa_gossip_transport::GossipTransport;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{oneshot, Mutex};

mod updater;

/// Saorsa Gossip CLI - Demonstrate and test gossip network features
#[derive(Parser, Debug)]
#[command(name = "saorsa-gossip")]
#[command(version, about = "Saorsa Gossip Network CLI Tool", long_about = None)]
struct Args {
    /// Config directory (default: ~/.saorsa-gossip)
    #[arg(short, long, default_value = "~/.saorsa-gossip")]
    config_dir: PathBuf,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Identity management (ML-DSA keypairs)
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },

    /// Network operations
    Network {
        #[command(subcommand)]
        action: NetworkAction,
    },

    /// Publish/Subscribe operations
    Pubsub {
        #[command(subcommand)]
        action: PubsubAction,
    },

    /// Presence beacon management
    Presence {
        #[command(subcommand)]
        action: PresenceAction,
    },

    /// Group operations
    Groups {
        #[command(subcommand)]
        action: GroupAction,
    },

    /// CRDT synchronization demo
    Crdt {
        #[command(subcommand)]
        action: CrdtAction,
    },

    /// Rendezvous coordination
    Rendezvous {
        #[command(subcommand)]
        action: RendezvousAction,
    },

    /// Run interactive demo
    Demo {
        /// Demo scenario to run
        #[arg(short, long, default_value = "basic")]
        scenario: String,
    },

    /// Check for and install updates
    Update {
        /// Check for updates without installing
        #[arg(short, long)]
        check_only: bool,
    },
}

#[derive(Subcommand, Debug)]
enum IdentityAction {
    /// Create a new identity
    Create {
        /// Alias for the identity
        #[arg(short, long)]
        alias: String,
    },

    /// List all identities
    List,

    /// Show identity details
    Show {
        /// Alias of identity to show
        alias: String,
    },

    /// Delete an identity
    Delete {
        /// Alias of identity to delete
        alias: String,
    },
}

#[derive(Subcommand, Debug)]
enum NetworkAction {
    /// Join the network
    Join {
        /// Coordinator address (e.g., 127.0.0.1:7000)
        #[arg(short, long)]
        coordinator: String,

        /// Identity alias to use
        #[arg(short, long)]
        identity: String,

        /// Bind address (default: 0.0.0.0:0 for random port)
        #[arg(short, long, default_value = "0.0.0.0:0")]
        bind: String,
    },

    /// Show network status
    Status,

    /// List known peers
    Peers,

    /// Leave the network
    Leave,
}

#[derive(Subcommand, Debug)]
enum PubsubAction {
    /// Subscribe to a topic
    Subscribe {
        /// Topic name
        #[arg(short, long)]
        topic: String,
    },

    /// Publish to a topic
    Publish {
        /// Topic name
        #[arg(short, long)]
        topic: String,

        /// Message to publish
        #[arg(short, long)]
        message: String,
    },

    /// Unsubscribe from a topic
    Unsubscribe {
        /// Topic name
        #[arg(short, long)]
        topic: String,
    },

    /// List subscribed topics
    List,
}

#[derive(Subcommand, Debug)]
enum PresenceAction {
    /// Start broadcasting presence
    Start {
        /// Topic for presence
        #[arg(short, long)]
        topic: String,
    },

    /// Stop broadcasting presence
    Stop {
        /// Topic to stop
        #[arg(short, long)]
        topic: String,
    },

    /// Show online peers
    Online {
        /// Topic to check
        #[arg(short, long)]
        topic: String,
    },
}

#[derive(Subcommand, Debug)]
enum GroupAction {
    /// Create a new group
    Create {
        /// Group name
        #[arg(short, long)]
        name: String,
    },

    /// Join a group
    Join {
        /// Group ID
        #[arg(short, long)]
        group_id: String,
    },

    /// Leave a group
    Leave {
        /// Group ID
        #[arg(short, long)]
        group_id: String,
    },

    /// List groups
    List,
}

#[derive(Subcommand, Debug)]
enum CrdtAction {
    /// Demonstrate LWW Register
    LwwRegister {
        /// Value to set
        value: String,
    },

    /// Demonstrate OR-Set
    OrSet {
        /// Action: add or remove
        #[arg(short, long)]
        action: String,

        /// Value
        value: String,
    },

    /// Show current CRDT state
    Show,
}

#[derive(Subcommand, Debug)]
enum RendezvousAction {
    /// Register as a provider
    Register {
        /// Capability to provide
        #[arg(short, long)]
        capability: String,
    },

    /// Find providers
    Find {
        /// Capability to find
        #[arg(short, long)]
        capability: String,
    },

    /// Unregister
    Unregister,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    init_logging(args.verbose)?;

    tracing::info!("Saorsa Gossip CLI v{}", env!("CARGO_PKG_VERSION"));

    // Expand config directory tilde
    let config_dir = expand_path(&args.config_dir)?;
    tracing::debug!("Config directory: {}", config_dir.display());

    // Ensure config directory exists
    tokio::fs::create_dir_all(&config_dir).await?;

    // Perform silent update check (rate-limited) for all commands except 'update'
    if !matches!(args.command, Commands::Update { .. }) {
        updater::silent_update_check(&config_dir).await;
    }

    // Route to command handlers
    match args.command {
        Commands::Identity { action } => handle_identity(action, &config_dir).await?,
        Commands::Network { action } => handle_network(action, &config_dir).await?,
        Commands::Pubsub { action } => handle_pubsub(action, &config_dir).await?,
        Commands::Presence { action } => handle_presence(action, &config_dir).await?,
        Commands::Groups { action } => handle_groups(action, &config_dir).await?,
        Commands::Crdt { action } => handle_crdt(action, &config_dir).await?,
        Commands::Rendezvous { action } => handle_rendezvous(action, &config_dir).await?,
        Commands::Demo { scenario } => handle_demo(&scenario, &config_dir).await?,
        Commands::Update { check_only } => handle_update(check_only).await?,
    }

    Ok(())
}

/// Handle identity commands
async fn handle_identity(action: IdentityAction, config_dir: &std::path::Path) -> Result<()> {
    use saorsa_gossip_identity::Identity;

    match action {
        IdentityAction::Create { alias } => {
            tracing::info!("Creating identity: {}", alias);

            let identity = Identity::new(alias.clone())?;
            let peer_id = identity.peer_id();

            // Save to keystore (using alias as four-words for now)
            let keystore = config_dir.join("keystore");
            let keystore_str = path_to_string(&keystore)?;
            identity.save_to_keystore(&alias, &keystore_str).await?;

            println!("✓ Created identity: {}", alias);
            println!("  PeerId: {}", hex::encode(peer_id.as_bytes()));
            println!("  Saved to: {}", keystore.display());
        }

        IdentityAction::List => {
            tracing::info!("Listing identities");
            let keystore = config_dir.join("keystore");

            if !keystore.exists() {
                println!("No identities found");
                return Ok(());
            }

            let mut entries = tokio::fs::read_dir(&keystore).await?;
            let mut count = 0;

            println!("Identities:");
            while let Some(entry) = entries.next_entry().await? {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".identity") {
                        let alias = name.trim_end_matches(".identity").replace('_', "-");
                        println!("  - {}", alias);
                        count += 1;
                    }
                }
            }

            if count == 0 {
                println!("  (none)");
            }
        }

        IdentityAction::Show { alias } => {
            tracing::info!("Showing identity: {}", alias);
            let keystore = config_dir.join("keystore");

            let identity =
                Identity::load_from_keystore(&alias, &path_to_string(&keystore)?).await?;

            println!("Identity: {}", alias);
            println!("  PeerId: {}", hex::encode(identity.peer_id().as_bytes()));
            println!("  Alias: {}", identity.alias());
        }

        IdentityAction::Delete { alias } => {
            tracing::info!("Deleting identity: {}", alias);
            let keystore = config_dir.join("keystore");
            let filename = alias.replace('-', "_");
            let file_path = keystore.join(format!("{}.identity", filename));

            if file_path.exists() {
                tokio::fs::remove_file(&file_path).await?;
                println!("✓ Deleted identity: {}", alias);
            } else {
                println!("Identity not found: {}", alias);
            }
        }
    }

    Ok(())
}

/// Handle network commands
async fn handle_network(action: NetworkAction, config_dir: &std::path::Path) -> Result<()> {
    use saorsa_gossip_identity::Identity;
    use saorsa_gossip_transport::{GossipStreamType, GossipTransport, UdpTransportAdapter};

    match action {
        NetworkAction::Join {
            coordinator,
            identity,
            bind,
        } => {
            tracing::info!("Joining network with identity: {}", identity);

            // Load identity
            let keystore = config_dir.join("keystore");
            let ident =
                Identity::load_from_keystore(&identity, &path_to_string(&keystore)?).await?;

            println!("✓ Loaded identity: {}", identity);
            println!("  PeerId: {}", hex::encode(ident.peer_id().as_bytes()));

            // Parse addresses
            let bind_addr: std::net::SocketAddr = bind.parse()?;
            let coordinator_addr: std::net::SocketAddr = coordinator.parse()?;

            println!("\n🌐 Connecting to network...");
            println!("  Coordinator: {}", coordinator);
            println!("  Local bind: {}", bind);

            // Create transport (automatically connects to known peers)
            println!("  Creating transport and establishing QUIC connection...");
            let transport =
                Arc::new(UdpTransportAdapter::new(bind_addr, vec![coordinator_addr]).await?);
            let coordinator_cache: CoordinatorAdvertCache = Arc::new(Mutex::new(HashMap::new()));
            let (pong_tx, pong_rx) = oneshot::channel();
            let listener_transport = transport.clone();
            tokio::spawn(run_transport_listener(
                listener_transport,
                Some(pong_tx),
                coordinator_cache,
            ));

            println!("\n✓ Transport initialized and connected!");
            println!(
                "  Transport PeerId: {}",
                hex::encode(transport.peer_id().as_bytes())
            );
            println!("  Ant PeerId: {:?}", transport.ant_peer_id());

            // Discover the coordinator peer ID via a bootstrap dial.
            println!("\n📡 Dialing coordinator bootstrap endpoint...");
            let coordinator_peer_id = match transport.dial_bootstrap(coordinator_addr).await {
                Ok(id) => {
                    println!("  Coordinator PeerId: {}", hex::encode(id.as_bytes()));
                    Some(id)
                }
                Err(e) => {
                    println!("❌ Failed to dial bootstrap node: {e}");
                    println!("   Continuing without ping; check coordinator logs.");
                    None
                }
            };

            if let Some(coordinator_peer_id) = coordinator_peer_id {
                // Send a PING to coordinator to test message exchange
                println!("\n📡 Sending PING to coordinator...");
                let ping_start = Instant::now();
                if let Err(e) = transport
                    .send_to_peer(
                        coordinator_peer_id,
                        GossipStreamType::Membership,
                        Bytes::from_static(b"PING"),
                    )
                    .await
                {
                    println!("❌ Failed to send PING: {e}");
                } else {
                    // Try to receive a message with timeout to test connectivity
                    println!("⏳ Waiting for coordinator response (5s timeout)...");

                    match tokio::time::timeout(std::time::Duration::from_secs(5), pong_rx).await {
                        Ok(Ok(())) => {
                            let rtt = ping_start.elapsed();
                            println!("✓ Received PONG in {:?}", rtt);
                        }
                        Ok(Err(_)) => {
                            println!("⚠️  Listener dropped before PONG notification");
                        }
                        Err(_) => {
                            println!("⏱️  Timeout waiting for response");
                        }
                    }
                }
            }

            println!("\n⚠️  Full network integration in progress!");
            println!("   - Address reflection (IPv4/IPv6 observation)");
            println!("   - NAT type detection");
            println!("   - Peer discovery and listing");
            println!("\nPress Ctrl+C to disconnect");

            // Keep connection alive
            tokio::signal::ctrl_c().await?;
            println!("\n👋 Disconnecting...");
        }

        NetworkAction::Status => {
            println!("Network status - Coming soon!");
            println!("This will show:");
            println!("  - Connection state");
            println!("  - Observed IPv4/IPv6 addresses");
            println!("  - NAT type");
            println!("  - Active peer count");
        }

        NetworkAction::Peers => {
            println!("Peer list - Coming soon!");
            println!("This will show:");
            println!("  - Connected peers with IPs (IPv4/IPv6)");
            println!("  - RTT to each peer");
            println!("  - Connection type (direct/relayed)");
        }

        NetworkAction::Leave => {
            println!("Leave network - Coming soon!");
        }
    }

    Ok(())
}

/// Handle pubsub commands
async fn handle_pubsub(_action: PubsubAction, _config_dir: &std::path::Path) -> Result<()> {
    println!("PubSub commands - Coming soon!");
    println!("This will demonstrate:");
    println!("  - Subscribing to topics");
    println!("  - Publishing messages");
    println!("  - Gossip-based message propagation");
    println!("  - ML-DSA signatures on messages");
    Ok(())
}

/// Handle presence commands
async fn handle_presence(_action: PresenceAction, _config_dir: &std::path::Path) -> Result<()> {
    println!("Presence commands - Coming soon!");
    println!("This will demonstrate:");
    println!("  - Periodic presence beacons");
    println!("  - Online peer discovery");
    println!("  - Presence TTL and expiration");
    Ok(())
}

/// Handle group commands
async fn handle_groups(_action: GroupAction, _config_dir: &std::path::Path) -> Result<()> {
    println!("Group commands - Coming soon!");
    println!("This will demonstrate:");
    println!("  - Creating encrypted groups");
    println!("  - Joining with shared secrets");
    println!("  - Group messaging");
    Ok(())
}

/// Handle CRDT commands
async fn handle_crdt(_action: CrdtAction, _config_dir: &std::path::Path) -> Result<()> {
    println!("CRDT commands - Coming soon!");
    println!("This will demonstrate:");
    println!("  - LWW Register operations");
    println!("  - OR-Set add/remove");
    println!("  - Anti-entropy synchronization");
    Ok(())
}

/// Handle rendezvous commands
async fn handle_rendezvous(_action: RendezvousAction, _config_dir: &std::path::Path) -> Result<()> {
    println!("Rendezvous commands - Coming soon!");
    println!("This will demonstrate:");
    println!("  - Provider registration");
    println!("  - Capability-based discovery");
    println!("  - DHT-based lookups");
    Ok(())
}

/// Handle demo scenarios
async fn handle_demo(scenario: &str, _config_dir: &std::path::Path) -> Result<()> {
    match scenario {
        "basic" => {
            println!("=== Saorsa Gossip Basic Demo ===");
            println!();
            println!("This demo will showcase:");
            println!("  1. Identity creation with ML-DSA");
            println!("  2. Network bootstrap");
            println!("  3. Peer discovery");
            println!("  4. PubSub messaging");
            println!("  5. Presence beacons");
            println!();
            println!("To run individual commands, use:");
            println!("  saorsa-gossip identity create --alias Alice");
            println!("  saorsa-gossip network join --coordinator 127.0.0.1:7000 --identity Alice");
            println!();
            println!("Demo implementation coming soon!");
        }
        _ => {
            println!("Unknown demo scenario: {}", scenario);
            println!("Available scenarios: basic");
        }
    }

    Ok(())
}

/// Initialize logging based on verbosity
fn init_logging(verbose: bool) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    let filter = if verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    Ok(())
}

/// Expand tilde in path
fn expand_path(path: &std::path::Path) -> Result<PathBuf> {
    let expanded = shellexpand::tilde(&path.to_string_lossy()).to_string();
    Ok(PathBuf::from(expanded))
}

fn path_to_string(path: &std::path::Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Path contains invalid UTF-8: {}", path.display()))
}

type CoordinatorAdvertCache = Arc<Mutex<HashMap<saorsa_gossip_types::PeerId, CoordinatorAdvert>>>;

async fn run_transport_listener(
    transport: Arc<saorsa_gossip_transport::UdpTransportAdapter>,
    mut pong_tx: Option<oneshot::Sender<()>>,
    cache: CoordinatorAdvertCache,
) {
    loop {
        match transport.receive_message().await {
            Ok((peer_id, stream_type, data)) => match stream_type {
                saorsa_gossip_transport::GossipStreamType::Membership => {
                    if data.as_ref() == b"PONG" {
                        println!("📨 PONG received from {}", hex::encode(peer_id.as_bytes()));
                        if let Some(tx) = pong_tx.take() {
                            let _ = tx.send(());
                        }
                    } else {
                        tracing::trace!(
                            peer = %hex::encode(peer_id.as_bytes()),
                            "Membership message ({} bytes)",
                            data.len()
                        );
                    }
                }
                saorsa_gossip_transport::GossipStreamType::PubSub => {
                    match CoordinatorAdvert::from_bytes(data.as_ref()) {
                        Ok(advert) => {
                            if !advert.is_valid() {
                                tracing::debug!(
                                    peer = %hex::encode(advert.peer.as_bytes()),
                                    "Discarding expired coordinator advert"
                                );
                                continue;
                            }

                            let mut cache_guard = cache.lock().await;
                            cache_guard.insert(advert.peer, advert.clone());
                            let known = cache_guard.len();
                            drop(cache_guard);

                            let addr_summary = if advert.addr_hints.is_empty() {
                                "<none>".to_string()
                            } else {
                                advert
                                    .addr_hints
                                    .iter()
                                    .map(|hint| hint.addr.to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            };

                            println!(
                                "📣 Coordinator advert from {} roles={:?} nat={:?} addrs=[{}] (known: {})",
                                hex::encode(advert.peer.as_bytes()),
                                advert.roles,
                                advert.nat_class,
                                addr_summary,
                                known
                            );
                        }
                        Err(_) => {
                            tracing::trace!(
                                peer = %hex::encode(peer_id.as_bytes()),
                                "Non-advert pubsub payload ({} bytes)",
                                data.len()
                            );
                        }
                    }
                }
                other => {
                    tracing::trace!(
                        peer = %hex::encode(peer_id.as_bytes()),
                        ?other,
                        "Ignoring gossip message ({} bytes)",
                        data.len()
                    );
                }
            },
            Err(e) => {
                tracing::debug!("Transport listener exiting: {e}");
                break;
            }
        }
    }
}

/// Handle update command
async fn handle_update(check_only: bool) -> Result<()> {
    if check_only {
        println!("🔍 Checking for updates...");
        match updater::check_for_update().await? {
            Some(new_version) => {
                println!("✓ Update available: {}", new_version);
                println!("  Run 'saorsa-gossip update' to install");
            }
            None => {
                println!("✓ Already on latest version: {}", env!("CARGO_PKG_VERSION"));
            }
        }
    } else {
        updater::perform_update().await?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parses() {
        // Verify CLI structure compiles and parses
        let _args = Args::try_parse_from(["saorsa-gossip", "demo", "--scenario", "basic"]);
    }

    #[test]
    fn test_expand_path_no_tilde() {
        let path = std::path::Path::new("/tmp/test");
        let expanded = expand_path(path).expect("expand");
        assert_eq!(expanded, path);
    }

    #[test]
    fn test_expand_path_with_tilde() {
        let path = std::path::Path::new("~/test");
        let expanded = expand_path(path).expect("expand");
        assert!(expanded.to_string_lossy().contains("test"));
        assert!(!expanded.to_string_lossy().contains('~'));
    }
}

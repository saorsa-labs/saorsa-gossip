//! Throughput test for Saorsa Gossip
//!
//! Tests data transfer performance from 10MB to 100MB with real-time statistics.
//!
//! Usage:
//! ```bash
//! # Terminal 1 - Run receiver (coordinator)
//! cargo run --example throughput_test --release -- receiver --bind 127.0.0.1:8000
//!
//! # Terminal 2 - Run sender (client)
//! cargo run --example throughput_test --release -- sender --coordinator 127.0.0.1:8000 --bind 127.0.0.1:9000
//! ```

use anyhow::{anyhow, Result};
use bytes::Bytes;
use saorsa_gossip_transport::{AntQuicTransport, GossipStreamType, GossipTransport};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Test configuration
const CHUNK_SIZES: &[usize] = &[
    10 * 1024 * 1024,  // 10 MB
    20 * 1024 * 1024,  // 20 MB
    50 * 1024 * 1024,  // 50 MB
    100 * 1024 * 1024, // 100 MB
];

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,saorsa_gossip=debug,ant_quic=info")
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  {} receiver --bind <ADDR>", args[0]);
        eprintln!("  {} sender --coordinator <ADDR> --bind <ADDR>", args[0]);
        std::process::exit(1);
    }

    match args[1].as_str() {
        "receiver" => run_receiver(&args[2..]).await,
        "sender" => run_sender(&args[2..]).await,
        _ => {
            eprintln!("Unknown mode: {}", args[1]);
            eprintln!("Use 'receiver' or 'sender'");
            std::process::exit(1);
        }
    }
}

async fn run_receiver(args: &[String]) -> Result<()> {
    let bind_addr = parse_bind_addr(args)?;

    println!("🎯 Throughput Test - RECEIVER");
    println!("================================");
    println!("Bind address: {}", bind_addr);
    println!();

    // Create transport (symmetric P2P node)
    println!("⏳ Initializing transport...");
    let transport = AntQuicTransport::new(bind_addr, vec![]).await?;

    let peer_id = transport.peer_id();
    println!("✓ Transport initialized");
    println!("  PeerId: {}", peer_id);
    println!();
    println!("📡 Waiting for sender to connect...");
    println!();

    // Receive test data
    let mut total_received = 0;
    let mut test_count = 0;

    loop {
        match transport.receive_message().await {
            Ok((sender_peer_id, stream_type, data)) => {
                test_count += 1;
                let size = data.len();
                total_received += size;

                println!("📦 Test #{} - Received data", test_count);
                println!("  Sender: {}", sender_peer_id);
                println!("  Stream: {:?}", stream_type);
                println!(
                    "  Size: {} bytes ({:.2} MB)",
                    size,
                    size as f64 / 1_048_576.0
                );
                println!(
                    "  Total received: {:.2} MB",
                    total_received as f64 / 1_048_576.0
                );
                println!();

                // Verify data integrity (all bytes should be the test number)
                let expected_byte = (test_count % 256) as u8;
                let all_correct = data.iter().all(|&b| b == expected_byte);
                if all_correct {
                    println!("  ✓ Data integrity verified");
                } else {
                    println!("  ⚠️  Data corruption detected!");
                }
                println!();
            }
            Err(e) => {
                eprintln!("❌ Receive error: {}", e);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn run_sender(args: &[String]) -> Result<()> {
    let (coordinator_addr, bind_addr) = parse_sender_args(args)?;

    println!("🚀 Throughput Test - SENDER");
    println!("================================");
    println!("Coordinator: {}", coordinator_addr);
    println!("Local bind: {}", bind_addr);
    println!();

    // Measure connection time
    println!("⏳ Connecting to coordinator...");
    let connect_start = Instant::now();

    let transport = AntQuicTransport::new(bind_addr, vec![coordinator_addr]).await?;

    let connect_duration = connect_start.elapsed();
    let peer_id = transport.peer_id();

    println!("✓ Connected in {:.3}s", connect_duration.as_secs_f64());
    println!("  Local PeerId: {}", peer_id);
    println!();

    // Get coordinator peer ID (we'll need to determine this - for now use a placeholder)
    // In practice, we'd query this from the connection
    // Get the coordinator's peer ID from the transport
    let coordinator_peer_id = transport
        .get_bootstrap_peer_id(coordinator_addr)
        .await
        .ok_or_else(|| anyhow!("Failed to retrieve coordinator peer ID"))?;

    println!("  Coordinator PeerId: {}", coordinator_peer_id);
    println!();
    println!("📊 Starting throughput tests...");
    println!();

    // Run tests for each chunk size
    for (test_num, &chunk_size) in CHUNK_SIZES.iter().enumerate() {
        let test_id = test_num + 1;

        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("📦 Test #{}: {} MB", test_id, chunk_size / 1_048_576);
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        // Generate test data (fill with test number for integrity check)
        let test_byte = (test_id % 256) as u8;
        let data = vec![test_byte; chunk_size];
        let data_bytes = Bytes::from(data);

        println!("  Preparing {} bytes...", chunk_size);
        println!("  Sending...");

        let send_start = Instant::now();

        match transport
            .send_to_peer(coordinator_peer_id, GossipStreamType::Bulk, data_bytes)
            .await
        {
            Ok(()) => {
                let send_duration = send_start.elapsed();
                let throughput_mbps =
                    (chunk_size as f64 * 8.0) / send_duration.as_secs_f64() / 1_000_000.0;
                let throughput_mbytes =
                    chunk_size as f64 / send_duration.as_secs_f64() / 1_048_576.0;

                println!();
                println!("  ✓ SUCCESS");
                println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                println!("  Duration:    {:.3}s", send_duration.as_secs_f64());
                println!("  Throughput:  {:.2} Mbps", throughput_mbps);
                println!("  Throughput:  {:.2} MB/s", throughput_mbytes);
                println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                println!();
            }
            Err(e) => {
                println!();
                println!("  ❌ FAILED: {}", e);
                println!();
            }
        }

        // Wait between tests
        if test_id < CHUNK_SIZES.len() {
            println!("⏸  Waiting 2s before next test...");
            sleep(Duration::from_secs(2)).await;
            println!();
        }
    }

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("✓ All tests completed!");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    Ok(())
}

fn parse_bind_addr(args: &[String]) -> Result<SocketAddr> {
    let mut bind_addr = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                if i + 1 < args.len() {
                    bind_addr = Some(args[i + 1].parse()?);
                    i += 2;
                } else {
                    anyhow::bail!("--bind requires an address");
                }
            }
            _ => {
                anyhow::bail!("Unknown argument: {}", args[i]);
            }
        }
    }

    bind_addr.ok_or_else(|| anyhow::anyhow!("Missing --bind argument"))
}

fn parse_sender_args(args: &[String]) -> Result<(SocketAddr, SocketAddr)> {
    let mut coordinator_addr = None;
    let mut bind_addr = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--coordinator" => {
                if i + 1 < args.len() {
                    coordinator_addr = Some(args[i + 1].parse()?);
                    i += 2;
                } else {
                    anyhow::bail!("--coordinator requires an address");
                }
            }
            "--bind" => {
                if i + 1 < args.len() {
                    bind_addr = Some(args[i + 1].parse()?);
                    i += 2;
                } else {
                    anyhow::bail!("--bind requires an address");
                }
            }
            _ => {
                anyhow::bail!("Unknown argument: {}", args[i]);
            }
        }
    }

    let coordinator =
        coordinator_addr.ok_or_else(|| anyhow::anyhow!("Missing --coordinator argument"))?;
    let bind = bind_addr.ok_or_else(|| anyhow::anyhow!("Missing --bind argument"))?;

    Ok((coordinator, bind))
}

// Allow deprecated transport types - this example demonstrates the current API
// which will be updated when Phase 3.3 migrates to ant-quic native transport.
#![allow(deprecated)]

//! Comprehensive Transport Benchmark
//!
//! Measures performance metrics for the Saorsa Gossip transport layer:
//! - Connection establishment time
//! - Throughput (MB/s and Mbps)
//! - Latency (round-trip time)
//! - Message success rate
//! - Per-transport metrics when using multiplexed mode
//!
//! Usage:
//! ```bash
//! # Terminal 1 - Run coordinator (receiver)
//! cargo run --example transport_benchmark --release -- coordinator --bind 127.0.0.1:8000
//!
//! # Terminal 2 - Run benchmark client (direct UDP transport)
//! cargo run --example transport_benchmark --release -- benchmark --coordinator 127.0.0.1:8000 --bind 127.0.0.1:9000
//!
//! # Terminal 2 - Run benchmark client (multiplexed transport mode)
//! cargo run --example transport_benchmark --release -- benchmark --coordinator 127.0.0.1:8000 --bind 127.0.0.1:9000 --multiplexed
//!
//! # Terminal 2 - Run with BLE stub transport (tests constrained link simulation)
//! cargo run --example transport_benchmark --release -- benchmark --coordinator 127.0.0.1:8000 --bind 127.0.0.1:9000 --multiplexed --ble
//! ```

use anyhow::Result;
use bytes::Bytes;
use saorsa_gossip_transport::{
    BleTransportAdapter, BleTransportAdapterConfig, BootstrapCache, BootstrapCacheConfig,
    GossipStreamType, GossipTransport, MultiplexedGossipTransport, TransportAdapter,
    TransportDescriptor, TransportMultiplexer, UdpTransportAdapter,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Benchmark configuration
const MESSAGE_SIZES: &[usize] = &[
    1024,              // 1 KB
    10 * 1024,         // 10 KB
    100 * 1024,        // 100 KB
    1024 * 1024,       // 1 MB
    10 * 1024 * 1024,  // 10 MB
    50 * 1024 * 1024,  // 50 MB
    100 * 1024 * 1024, // 100 MB
];

#[allow(dead_code)]
const CONCURRENT_CONNECTIONS: &[usize] = &[1, 5, 10, 20];
const MESSAGES_PER_SIZE: usize = 10;

#[derive(Debug, Clone)]
struct BenchmarkResult {
    message_size: usize,
    duration: Duration,
    throughput_mbps: f64,
    throughput_mbytes: f64,
    success: bool,
}

#[derive(Debug, Clone)]
struct ConnectionStats {
    connection_time: Duration,
    peer_id: String,
}

#[derive(Debug)]
struct BenchmarkSummary {
    total_messages: usize,
    successful_messages: usize,
    failed_messages: usize,
    total_bytes_sent: usize,
    #[allow(dead_code)]
    total_duration: Duration,
    #[allow(dead_code)]
    avg_throughput_mbps: f64,
    #[allow(dead_code)]
    avg_throughput_mbytes: f64,
    connection_stats: Vec<ConnectionStats>,
    results: Vec<BenchmarkResult>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,saorsa_gossip=debug,ant_quic=info")
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  {} coordinator --bind <ADDR>", args[0]);
        eprintln!("  {} benchmark --coordinator <ADDR> --bind <ADDR>", args[0]);
        std::process::exit(1);
    }

    match args[1].as_str() {
        "coordinator" => run_coordinator(&args[2..]).await,
        "benchmark" => run_benchmark(&args[2..]).await,
        _ => {
            eprintln!("Unknown mode: {}", args[1]);
            eprintln!("Use 'coordinator' or 'benchmark'");
            std::process::exit(1);
        }
    }
}

async fn run_coordinator(args: &[String]) -> Result<()> {
    let bind_addr = parse_bind_addr(args)?;

    println!("🎯 Transport Benchmark - COORDINATOR");
    println!("====================================");
    println!("Bind address: {}", bind_addr);
    println!();

    // Create bootstrap cache for coordinator using ant-quic's BootstrapCache
    let cache_dir = std::env::temp_dir().join("saorsa-benchmark-coordinator");
    let cache_config = BootstrapCacheConfig::builder()
        .cache_dir(&cache_dir)
        .max_peers(10000)
        .build();
    let cache = Arc::new(BootstrapCache::open(cache_config).await?);

    // Create transport (symmetric P2P node)
    println!("⏳ Initializing transport with bootstrap cache...");
    let transport =
        UdpTransportAdapter::new_with_cache(bind_addr, vec![], Some(Arc::clone(&cache))).await?;

    let peer_id = transport.peer_id();
    println!("✓ Transport initialized");
    println!("  PeerId: {}", peer_id);

    // Display cache stats
    let stats = cache.stats().await;
    println!(
        "  Cache: {} total peers ({} relay, {} coordinator)",
        stats.total_peers, stats.relay_peers, stats.coordinator_peers
    );
    println!();
    println!("📡 Waiting for benchmark clients...");
    println!();

    // Receive and log messages
    let mut total_received = 0;
    let mut message_count = 0;
    let start_time = Instant::now();

    loop {
        match transport.receive_message().await {
            Ok((sender_peer_id, stream_type, data)) => {
                message_count += 1;
                let size = data.len();
                total_received += size;
                let elapsed = start_time.elapsed();

                println!("📦 Message #{}", message_count);
                println!("  From: {}", sender_peer_id);
                println!("  Stream: {:?}", stream_type);
                println!(
                    "  Size: {} bytes ({:.2} MB)",
                    size,
                    size as f64 / 1_048_576.0
                );
                println!(
                    "  Total: {:.2} MB in {:.2}s",
                    total_received as f64 / 1_048_576.0,
                    elapsed.as_secs_f64()
                );

                // Calculate current throughput
                let throughput_mbps =
                    (total_received as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
                let throughput_mbytes = total_received as f64 / elapsed.as_secs_f64() / 1_048_576.0;
                println!(
                    "  Avg Throughput: {:.2} Mbps ({:.2} MB/s)",
                    throughput_mbps, throughput_mbytes
                );
                println!();
            }
            Err(e) => {
                eprintln!("❌ Receive error: {}", e);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn run_benchmark(args: &[String]) -> Result<()> {
    let benchmark_args = parse_benchmark_args(args)?;
    let coordinator_addr = benchmark_args.coordinator_addr;
    let bind_addr = benchmark_args.bind_addr;

    println!("🚀 Transport Benchmark - CLIENT");
    println!("================================");
    println!("Coordinator: {}", coordinator_addr);
    println!("Local bind: {}", bind_addr);
    if benchmark_args.multiplexed {
        if benchmark_args.ble_enabled {
            println!("Mode: MULTIPLEXED TRANSPORT (UDP + BLE stub)");
        } else {
            println!("Mode: MULTIPLEXED TRANSPORT");
        }
    } else {
        println!("Mode: DIRECT UDP TRANSPORT");
    }
    println!();

    // Create bootstrap cache for benchmark client using ant-quic's BootstrapCache
    let cache_dir = std::env::temp_dir().join("saorsa-benchmark-client");
    let cache_config = BootstrapCacheConfig::builder()
        .cache_dir(&cache_dir)
        .max_peers(10000)
        .build();
    let cache = Arc::new(BootstrapCache::open(cache_config).await?);

    // Measure connection time
    if benchmark_args.multiplexed {
        println!("⏳ Connecting to coordinator via MULTIPLEXED transport...");
    } else {
        println!("⏳ Connecting to coordinator with bootstrap cache...");
    }
    let connect_start = Instant::now();

    // Create transport - either direct UDP or multiplexed
    let (transport, peer_id, coordinator_peer_id, ble_adapter_opt): (
        Arc<dyn GossipTransport>,
        saorsa_gossip_types::PeerId,
        saorsa_gossip_types::PeerId,
        Option<Arc<BleTransportAdapter>>,
    ) = if benchmark_args.multiplexed {
        // Create multiplexed transport wrapping UDP adapter
        let udp_adapter = Arc::new(
            UdpTransportAdapter::new_with_cache(
                bind_addr,
                vec![coordinator_addr],
                Some(Arc::clone(&cache)),
            )
            .await?,
        );

        let local_peer_id = udp_adapter.peer_id();

        // Get coordinator peer ID before wrapping
        let coord_peer_id = udp_adapter
            .get_bootstrap_peer_id(coordinator_addr)
            .await
            .ok_or_else(|| anyhow::anyhow!("Failed to retrieve coordinator peer ID"))?;

        // Create multiplexer and register UDP transport
        let multiplexer = TransportMultiplexer::new(local_peer_id);
        multiplexer
            .register_transport(TransportDescriptor::Udp, udp_adapter)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to register UDP transport: {}", e))?;
        multiplexer
            .set_default_transport(TransportDescriptor::Udp)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to set default transport: {}", e))?;

        // Optionally add BLE stub transport
        let ble_adapter = if benchmark_args.ble_enabled {
            let ble_config = BleTransportAdapterConfig::new()
                .with_latency(50, 150) // 50-150ms simulated latency
                .with_mtu(512); // BLE typical MTU
            let ble = Arc::new(BleTransportAdapter::with_config(local_peer_id, ble_config));

            multiplexer
                .register_transport(TransportDescriptor::Ble, Arc::clone(&ble) as Arc<_>)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to register BLE transport: {}", e))?;

            println!("  BLE stub registered (MTU: 512, latency: 50-150ms)");
            Some(ble)
        } else {
            None
        };

        // Wrap in MultiplexedGossipTransport
        let multiplexed = MultiplexedGossipTransport::new(Arc::new(multiplexer), local_peer_id);

        (
            Arc::new(multiplexed),
            local_peer_id,
            coord_peer_id,
            ble_adapter,
        )
    } else {
        // Direct UDP transport (original mode)
        let transport = UdpTransportAdapter::new_with_cache(
            bind_addr,
            vec![coordinator_addr],
            Some(Arc::clone(&cache)),
        )
        .await?;

        let local_peer_id = transport.peer_id();
        let coord_peer_id = transport
            .get_bootstrap_peer_id(coordinator_addr)
            .await
            .ok_or_else(|| anyhow::anyhow!("Failed to retrieve coordinator peer ID"))?;

        (Arc::new(transport), local_peer_id, coord_peer_id, None)
    };

    let connect_duration = connect_start.elapsed();

    println!("✓ Connected in {:.3}s", connect_duration.as_secs_f64());
    println!("  Local PeerId: {}", peer_id);

    // Display cache stats
    let stats = cache.stats().await;
    println!(
        "  Cache: {} total peers ({} relay, {} coordinator)",
        stats.total_peers, stats.relay_peers, stats.coordinator_peers
    );
    if benchmark_args.multiplexed {
        println!("  Transport Mode: Multiplexed (UDP via TransportMultiplexer)");
    }
    println!();

    println!("  Coordinator PeerId: {}", coordinator_peer_id);
    println!();

    // Initialize benchmark summary
    let mut summary = BenchmarkSummary {
        total_messages: 0,
        successful_messages: 0,
        failed_messages: 0,
        total_bytes_sent: 0,
        total_duration: Duration::ZERO,
        avg_throughput_mbps: 0.0,
        avg_throughput_mbytes: 0.0,
        connection_stats: vec![ConnectionStats {
            connection_time: connect_duration,
            peer_id: coordinator_peer_id.to_string(),
        }],
        results: Vec::new(),
    };

    println!("📊 Starting Comprehensive Benchmark");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    // Run benchmarks for each message size
    for (idx, &message_size) in MESSAGE_SIZES.iter().enumerate() {
        println!(
            "┌─ Test {}/{}: {} bytes ({:.2} MB)",
            idx + 1,
            MESSAGE_SIZES.len(),
            message_size,
            message_size as f64 / 1_048_576.0
        );
        println!("│");

        let mut size_results = Vec::new();

        for iteration in 1..=MESSAGES_PER_SIZE {
            // Generate test data
            let test_byte = ((idx * MESSAGES_PER_SIZE + iteration) % 256) as u8;
            let data = vec![test_byte; message_size];
            let data_bytes = Bytes::from(data);

            print!("│  [{}/{}] Sending... ", iteration, MESSAGES_PER_SIZE);
            std::io::Write::flush(&mut std::io::stdout()).ok();

            let send_start = Instant::now();
            let result = transport
                .send_to_peer(coordinator_peer_id, GossipStreamType::Bulk, data_bytes)
                .await;

            let send_duration = send_start.elapsed();

            match result {
                Ok(()) => {
                    let throughput_mbps =
                        (message_size as f64 * 8.0) / send_duration.as_secs_f64() / 1_000_000.0;
                    let throughput_mbytes =
                        message_size as f64 / send_duration.as_secs_f64() / 1_048_576.0;

                    println!(
                        "✓ {:.3}s ({:.2} Mbps, {:.2} MB/s)",
                        send_duration.as_secs_f64(),
                        throughput_mbps,
                        throughput_mbytes
                    );

                    size_results.push(BenchmarkResult {
                        message_size,
                        duration: send_duration,
                        throughput_mbps,
                        throughput_mbytes,
                        success: true,
                    });

                    summary.successful_messages += 1;
                    summary.total_bytes_sent += message_size;
                }
                Err(e) => {
                    println!("✗ Failed: {}", e);
                    size_results.push(BenchmarkResult {
                        message_size,
                        duration: send_duration,
                        throughput_mbps: 0.0,
                        throughput_mbytes: 0.0,
                        success: false,
                    });
                    summary.failed_messages += 1;
                }
            }

            summary.total_messages += 1;

            // Small delay between messages
            sleep(Duration::from_millis(100)).await;
        }

        // Calculate statistics for this message size
        let successful: Vec<_> = size_results.iter().filter(|r| r.success).collect();
        if !successful.is_empty() {
            let avg_mbps: f64 =
                successful.iter().map(|r| r.throughput_mbps).sum::<f64>() / successful.len() as f64;
            let avg_mbytes: f64 = successful.iter().map(|r| r.throughput_mbytes).sum::<f64>()
                / successful.len() as f64;
            let avg_duration: f64 = successful
                .iter()
                .map(|r| r.duration.as_secs_f64())
                .sum::<f64>()
                / successful.len() as f64;

            println!("│");
            println!("│  Summary:");
            println!("│    Success: {}/{}", successful.len(), MESSAGES_PER_SIZE);
            println!("│    Avg Duration: {:.3}s", avg_duration);
            println!(
                "│    Avg Throughput: {:.2} Mbps ({:.2} MB/s)",
                avg_mbps, avg_mbytes
            );
        }

        summary.results.extend(size_results);
        println!("└─");
        println!();

        // Wait between different message sizes
        if idx < MESSAGE_SIZES.len() - 1 {
            sleep(Duration::from_millis(500)).await;
        }
    }

    // Display final summary
    display_summary(&summary);

    // Display BLE transport statistics if enabled
    if let Some(ref ble) = ble_adapter_opt {
        println!("📡 BLE Transport Statistics");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Messages sent: {}", ble.messages_sent().await);
        println!("  Bytes sent: {} bytes", ble.bytes_sent().await);
        println!("  MTU: {} bytes", ble.capabilities().max_message_size);
        println!(
            "  Typical latency: {}ms",
            ble.capabilities().typical_latency_ms
        );
        println!();
    }

    // Display cache stats after benchmark
    let final_stats = cache.stats().await;
    println!("📦 Final Cache Stats");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Total peers: {}", final_stats.total_peers);
    println!("  Relay peers: {}", final_stats.relay_peers);
    println!("  Coordinator peers: {}", final_stats.coordinator_peers);
    println!("  Average quality: {:.2}", final_stats.average_quality);
    println!();

    Ok(())
}

fn display_summary(summary: &BenchmarkSummary) {
    println!("🎉 BENCHMARK COMPLETE");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    // Overall statistics
    println!("📊 Overall Statistics");
    println!("────────────────────────────────────────");
    println!("  Total Messages: {}", summary.total_messages);
    println!(
        "  Successful: {} ({:.1}%)",
        summary.successful_messages,
        (summary.successful_messages as f64 / summary.total_messages as f64) * 100.0
    );
    println!("  Failed: {}", summary.failed_messages);
    println!(
        "  Total Data Sent: {:.2} MB",
        summary.total_bytes_sent as f64 / 1_048_576.0
    );
    println!();

    // Connection statistics
    println!("🔌 Connection Statistics");
    println!("────────────────────────────────────────");
    for (idx, conn) in summary.connection_stats.iter().enumerate() {
        println!(
            "  Connection {}: {:.3}s (PeerId: {})",
            idx + 1,
            conn.connection_time.as_secs_f64(),
            &conn.peer_id[..16]
        );
    }
    println!();

    // Throughput by message size
    println!("📈 Throughput by Message Size");
    println!("────────────────────────────────────────");

    for &size in MESSAGE_SIZES {
        let size_results: Vec<_> = summary
            .results
            .iter()
            .filter(|r| r.message_size == size && r.success)
            .collect();

        if !size_results.is_empty() {
            let avg_mbps = size_results.iter().map(|r| r.throughput_mbps).sum::<f64>()
                / size_results.len() as f64;
            let avg_mbytes = size_results
                .iter()
                .map(|r| r.throughput_mbytes)
                .sum::<f64>()
                / size_results.len() as f64;
            let min_mbps = size_results
                .iter()
                .map(|r| r.throughput_mbps)
                .min_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap_or(0.0);
            let max_mbps = size_results
                .iter()
                .map(|r| r.throughput_mbps)
                .max_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap_or(0.0);

            println!("  {} bytes ({} KB):", size, size / 1024);
            println!("    Avg: {:.2} Mbps ({:.2} MB/s)", avg_mbps, avg_mbytes);
            println!("    Min: {:.2} Mbps", min_mbps);
            println!("    Max: {:.2} Mbps", max_mbps);
            println!();
        }
    }

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
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

/// Benchmark arguments including optional multiplexed mode
struct BenchmarkArgs {
    coordinator_addr: SocketAddr,
    bind_addr: SocketAddr,
    multiplexed: bool,
    /// Enable BLE transport stub (requires --multiplexed)
    ble_enabled: bool,
}

fn parse_benchmark_args(args: &[String]) -> Result<BenchmarkArgs> {
    let mut coordinator_addr = None;
    let mut bind_addr = None;
    let mut multiplexed = false;
    let mut ble_enabled = false;

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
            "--multiplexed" => {
                multiplexed = true;
                i += 1;
            }
            "--ble" => {
                ble_enabled = true;
                i += 1;
            }
            _ => {
                anyhow::bail!("Unknown argument: {}", args[i]);
            }
        }
    }

    let coordinator =
        coordinator_addr.ok_or_else(|| anyhow::anyhow!("Missing --coordinator argument"))?;
    let bind = bind_addr.ok_or_else(|| anyhow::anyhow!("Missing --bind argument"))?;

    // --ble requires --multiplexed
    if ble_enabled && !multiplexed {
        anyhow::bail!("--ble requires --multiplexed mode");
    }

    Ok(BenchmarkArgs {
        coordinator_addr: coordinator,
        bind_addr: bind,
        multiplexed,
        ble_enabled,
    })
}

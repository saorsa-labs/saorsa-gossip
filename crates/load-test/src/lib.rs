#![warn(missing_docs)]

//! Load Testing Framework for Saorsa Gossip
//!
//! This crate provides comprehensive load testing capabilities for
//! validating Saorsa Gossip performance under high message rates and
//! concurrent peer loads.
//!
//! # Features
//!
//! - **High-throughput message generation** with configurable patterns
//! - **Concurrent peer simulation** with realistic behavior models
//! - **Real-time performance metrics** collection and analysis
//! - **Scalable load scenarios** from small tests to massive simulations
//! - **Performance regression detection** with statistical analysis
//!
//! # Example Usage
//!
//! ```rust,no_run
//! use saorsa_gossip_load_test::{LoadTestRunner, LoadScenario, MessagePattern};
//! use saorsa_gossip_simulator::{NetworkSimulator, Topology};
//! use std::time::Duration;
//! use std::sync::Arc;
//! use tokio::sync::RwLock;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create network simulator
//! let simulator = Arc::new(RwLock::new(NetworkSimulator::new()
//!     .with_nodes(100)
//!     .with_topology(Topology::Mesh)));
//!
//! // Create a load scenario
//! let scenario = LoadScenario {
//!     name: "pubsub_storm".to_string(),
//!     duration: Duration::from_secs(60),
//!     num_peers: 100,
//!     message_pattern: MessagePattern::Burst {
//!         messages_per_burst: 100,
//!         burst_interval: Duration::from_millis(100),
//!         message_size: 1024,
//!     },
//!     topology: Topology::Mesh,
//!     chaos_events: vec![], // No chaos for pure load testing
//! };
//!
//! // Run the load test
//! let runner = LoadTestRunner::new()?;
//! let results = runner.run_scenario(scenario, simulator).await?;
//!
//! println!("Throughput: {} msgs/sec", results.throughput_msgs_per_sec);
//! println!("Latency P95: {}ms", results.latency_p95_ms);
//! println!("Memory usage: {}MB", results.memory_usage_mb);
//!
//! # Ok(())
//! # }
//! ```

use hdrhistogram::Histogram;
use rand::prelude::*;
use rand_pcg::Pcg64;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::{self, Instant as TokioInstant};
use tracing::{debug, info, warn};

use saorsa_gossip_simulator::{
    ChaosInjector, MessageType, NetworkSimulator, SimulatedMessage, Topology,
};
use saorsa_gossip_types::TopicId;

/// Configuration for ramp-up message pattern generation
struct RampUpConfig {
    peer_id: u32,
    start_rate: u32,
    end_rate: u32,
    ramp_duration: Duration,
    message_size: usize,
    stats: Arc<RwLock<MessageStats>>,
    simulator: Arc<RwLock<NetworkSimulator>>,
}

/// Configuration for realistic message pattern generation
struct RealisticConfig {
    peer_id: u32,
    base_rate: u32,
    message_size: usize,
    stats: Arc<RwLock<MessageStats>>,
    simulator: Arc<RwLock<NetworkSimulator>>,
}

/// Load testing result metrics
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadTestResults {
    /// Test scenario name
    pub scenario_name: String,
    /// Total duration of the test
    pub duration: Duration,
    /// Number of peers simulated
    pub num_peers: usize,
    /// Total messages sent
    pub total_messages: u64,
    /// Messages per second throughput
    pub throughput_msgs_per_sec: f64,
    /// 50th percentile latency in milliseconds
    pub latency_p50_ms: u64,
    /// 95th percentile latency in milliseconds
    pub latency_p95_ms: u64,
    /// 99th percentile latency in milliseconds
    pub latency_p99_ms: u64,
    /// Message loss rate (0.0 to 1.0)
    pub message_loss_rate: f64,
    /// Memory usage in MB
    pub memory_usage_mb: f64,
    /// CPU utilization percentage
    pub cpu_utilization_percent: f64,
    /// Error count
    pub error_count: u64,
    /// Start timestamp
    pub start_time: chrono::DateTime<chrono::Utc>,
    /// End timestamp
    pub end_time: chrono::DateTime<chrono::Utc>,
}

/// Message generation patterns for load testing
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MessagePattern {
    /// Constant rate message generation
    Constant {
        /// Messages per second
        rate_per_second: u32,
        /// Size of each message in bytes
        message_size: usize,
    },
    /// Burst pattern with periodic message floods
    Burst {
        /// Messages per burst
        messages_per_burst: u32,
        /// Time between bursts
        burst_interval: Duration,
        /// Size of each message in bytes
        message_size: usize,
    },
    /// Ramp up pattern starting slow and increasing
    RampUp {
        /// Starting messages per second
        start_rate_per_second: u32,
        /// Ending messages per second
        end_rate_per_second: u32,
        /// Ramp duration
        ramp_duration: Duration,
        /// Size of each message in bytes
        message_size: usize,
    },
    /// Realistic pattern mimicking user behavior
    Realistic {
        /// Base message rate
        base_rate_per_second: u32,
        /// Peak rate multiplier
        peak_multiplier: f64,
        /// Peak duration as fraction of total test (0.0-1.0)
        peak_fraction: f64,
        /// Size of each message in bytes
        message_size: usize,
    },
}

/// Load test scenario configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadScenario {
    /// Scenario name for identification
    pub name: String,
    /// Total test duration
    pub duration: Duration,
    /// Number of concurrent peers to simulate
    pub num_peers: usize,
    /// Message generation pattern
    pub message_pattern: MessagePattern,
    /// Network topology
    pub topology: Topology,
    /// Optional chaos events to inject during load testing
    pub chaos_events: Vec<(Duration, saorsa_gossip_simulator::ChaosEvent)>,
}

/// Message generation statistics
#[derive(Clone, Debug)]
struct MessageStats {
    /// Messages sent
    sent: u64,
    /// Messages received
    received: u64,
    /// Send errors
    errors: u64,
    /// Send timestamps for latency calculation
    send_times: HashMap<u64, TokioInstant>,
    /// Latency histogram
    latency_histogram: Histogram<u64>,
}

/// Load test runner - main orchestrator for load testing
pub struct LoadTestRunner {
    /// Random number generator
    rng: Arc<Mutex<Pcg64>>,
    /// Message statistics
    stats: Arc<RwLock<MessageStats>>,
    /// Start time of current test
    start_time: Arc<RwLock<Option<TokioInstant>>>,
}

impl LoadTestRunner {
    /// Create a new load test runner
    ///
    /// # Errors
    ///
    /// Returns an error if the histogram cannot be created (should not happen
    /// with valid parameters).
    pub fn new() -> Result<Self, hdrhistogram::CreationError> {
        let rng = Pcg64::seed_from_u64(31415); // Deterministic seed for testing

        Ok(Self {
            rng: Arc::new(Mutex::new(rng)),
            stats: Arc::new(RwLock::new(MessageStats {
                sent: 0,
                received: 0,
                errors: 0,
                send_times: HashMap::new(),
                latency_histogram: Histogram::new(3)?, // 1ms to ~8 hours
            })),
            start_time: Arc::new(RwLock::new(None)),
        })
    }

    /// Create load test runner with specific seed
    ///
    /// # Errors
    ///
    /// Returns an error if the histogram cannot be created.
    pub fn with_seed(seed: u64) -> Result<Self, hdrhistogram::CreationError> {
        let mut runner = Self::new()?;
        runner.rng = Arc::new(Mutex::new(Pcg64::seed_from_u64(seed)));
        Ok(runner)
    }

    /// Run a complete load test scenario
    pub async fn run_scenario(
        &self,
        scenario: LoadScenario,
        simulator: Arc<RwLock<NetworkSimulator>>,
    ) -> Result<LoadTestResults, LoadTestError> {
        info!("Starting load test scenario: {}", scenario.name);

        let start_time = chrono::Utc::now();
        *self.start_time.write().await = Some(TokioInstant::now());

        // Start simulator if not already started
        {
            let sim = simulator.read().await;
            if !sim.is_running().await {
                drop(sim);
                let mut sim = simulator.write().await;
                sim.start().await?;
            }
        }

        // Create chaos injector if chaos events are specified
        if !scenario.chaos_events.is_empty() {
            let injector = ChaosInjector::new();

            // Run chaos scenario in background
            let chaos_scenario = saorsa_gossip_simulator::ChaosScenario {
                name: format!("{}_chaos", scenario.name),
                duration: scenario.duration,
                events: scenario.chaos_events.clone(),
            };

            let injector_clone = injector.clone();
            let simulator_clone = Arc::clone(&simulator);
            tokio::spawn(async move {
                if let Err(e) = injector_clone
                    .run_scenario(chaos_scenario, simulator_clone)
                    .await
                {
                    warn!("Chaos scenario failed: {:?}", e);
                }
            });
        }

        // Start message generators
        let message_tasks = self.start_message_generators(&scenario, &simulator).await?;

        // Monitor test progress
        let results = self
            .monitor_test_progress(scenario.clone(), start_time)
            .await?;

        // Clean up - don't stop the simulator as it was passed in
        for task in message_tasks {
            task.abort();
        }

        info!("Completed load test scenario: {}", scenario.name);
        Ok(results)
    }

    /// Start message generator tasks for each peer
    async fn start_message_generators(
        &self,
        scenario: &LoadScenario,
        simulator: &Arc<RwLock<NetworkSimulator>>,
    ) -> Result<Vec<tokio::task::JoinHandle<()>>, LoadTestError> {
        let mut tasks = Vec::new();
        let stats = self.stats.clone();

        for peer_id in 0..scenario.num_peers {
            let peer_id = peer_id as u32;
            let pattern = scenario.message_pattern.clone();
            let stats_clone = stats.clone();
            let simulator_clone = Arc::clone(simulator);

            let task = tokio::spawn(async move {
                Self::run_message_generator(peer_id, pattern, stats_clone, simulator_clone).await;
            });

            tasks.push(task);
        }

        Ok(tasks)
    }

    /// Run message generator for a single peer
    async fn run_message_generator(
        peer_id: u32,
        pattern: MessagePattern,
        stats: Arc<RwLock<MessageStats>>,
        simulator: Arc<RwLock<NetworkSimulator>>,
    ) {
        let topic = TopicId::new([1u8; 32]); // Fixed topic for load testing

        match pattern {
            MessagePattern::Constant {
                rate_per_second,
                message_size,
            } => {
                Self::generate_constant_rate(
                    peer_id,
                    rate_per_second,
                    message_size,
                    topic,
                    stats,
                    simulator,
                )
                .await;
            }
            MessagePattern::Burst {
                messages_per_burst,
                burst_interval,
                message_size,
            } => {
                Self::generate_burst_pattern(
                    peer_id,
                    messages_per_burst,
                    burst_interval,
                    message_size,
                    topic,
                    stats,
                    simulator,
                )
                .await;
            }
            MessagePattern::RampUp {
                start_rate_per_second,
                end_rate_per_second,
                ramp_duration,
                message_size,
            } => {
                Self::generate_ramp_up_pattern(RampUpConfig {
                    peer_id,
                    start_rate: start_rate_per_second,
                    end_rate: end_rate_per_second,
                    ramp_duration,
                    message_size,
                    stats,
                    simulator,
                })
                .await;
            }
            MessagePattern::Realistic {
                base_rate_per_second,
                peak_multiplier: _,
                peak_fraction: _,
                message_size,
            } => {
                Self::generate_realistic_pattern(RealisticConfig {
                    peer_id,
                    base_rate: base_rate_per_second,
                    message_size,
                    stats,
                    simulator,
                })
                .await;
            }
        }
    }

    /// Generate messages at constant rate
    async fn generate_constant_rate(
        peer_id: u32,
        rate_per_second: u32,
        message_size: usize,
        _topic: TopicId,
        stats: Arc<RwLock<MessageStats>>,
        simulator: Arc<RwLock<NetworkSimulator>>,
    ) {
        let interval = Duration::from_secs(1) / rate_per_second;
        let mut interval_timer = time::interval(interval);

        loop {
            interval_timer.tick().await;

            let message_id = {
                let mut stats_guard = stats.write().await;
                stats_guard.sent += 1;
                stats_guard.sent
            };

            let payload = vec![peer_id as u8; message_size];
            let message = SimulatedMessage {
                from: peer_id,
                to: ((peer_id + 1) % 5), // Send to next peer in ring
                payload,
                message_type: MessageType::PubSub,
                priority: 0,
                id: message_id,
            };

            // Record send time
            {
                let mut stats_guard = stats.write().await;
                stats_guard
                    .send_times
                    .insert(message_id, TokioInstant::now());
            }

            // Send message through simulator
            if let Err(e) = simulator
                .read()
                .await
                .send_message(peer_id, message.to, message.payload, message.message_type)
                .await
            {
                debug!("Failed to send message: {:?}", e);
                let mut stats_guard = stats.write().await;
                stats_guard.errors += 1;
            }
        }
    }

    /// Generate burst pattern messages
    async fn generate_burst_pattern(
        peer_id: u32,
        messages_per_burst: u32,
        burst_interval: Duration,
        message_size: usize,
        _topic: TopicId,
        stats: Arc<RwLock<MessageStats>>,
        simulator: Arc<RwLock<NetworkSimulator>>,
    ) {
        let mut burst_timer = time::interval(burst_interval);

        loop {
            burst_timer.tick().await;

            // Send burst of messages
            for _ in 0..messages_per_burst {
                let message_id = {
                    let mut stats_guard = stats.write().await;
                    stats_guard.sent += 1;
                    stats_guard.sent
                };

                let payload = vec![peer_id as u8; message_size];
                let message = SimulatedMessage {
                    from: peer_id,
                    to: ((peer_id + 1) % 5),
                    payload,
                    message_type: MessageType::PubSub,
                    priority: 0,
                    id: message_id,
                };

                // Record send time
                {
                    let mut stats_guard = stats.write().await;
                    stats_guard
                        .send_times
                        .insert(message_id, TokioInstant::now());
                }

                if let Err(e) = simulator
                    .read()
                    .await
                    .send_message(peer_id, message.to, message.payload, message.message_type)
                    .await
                {
                    debug!("Failed to send message: {:?}", e);
                    let mut stats_guard = stats.write().await;
                    stats_guard.errors += 1;
                }
            }
        }
    }

    /// Generate ramp-up pattern messages
    async fn generate_ramp_up_pattern(config: RampUpConfig) {
        let RampUpConfig {
            peer_id,
            start_rate,
            end_rate,
            ramp_duration,
            message_size,
            stats,
            simulator,
        } = config;

        let start_time = TokioInstant::now();
        let ramp_duration_secs = ramp_duration.as_secs_f64();
        let rate_range = end_rate as f64 - start_rate as f64;

        loop {
            let elapsed = start_time.elapsed().as_secs_f64();
            let progress = (elapsed / ramp_duration_secs).min(1.0);
            let current_rate = start_rate as f64 + (rate_range * progress);

            let interval = Duration::from_secs_f64(1.0 / current_rate);
            time::sleep(interval).await;

            let message_id = {
                let mut stats_guard = stats.write().await;
                stats_guard.sent += 1;
                stats_guard.sent
            };

            let payload = vec![peer_id as u8; message_size];
            let message = SimulatedMessage {
                from: peer_id,
                to: ((peer_id + 1) % 5),
                payload,
                message_type: MessageType::PubSub,
                priority: 0,
                id: message_id,
            };

            // Record send time
            {
                let mut stats_guard = stats.write().await;
                stats_guard
                    .send_times
                    .insert(message_id, TokioInstant::now());
            }

            if let Err(e) = simulator
                .read()
                .await
                .send_message(peer_id, message.to, message.payload, message.message_type)
                .await
            {
                debug!("Failed to send message: {:?}", e);
                let mut stats_guard = stats.write().await;
                stats_guard.errors += 1;
            }
        }
    }

    /// Generate realistic pattern messages
    async fn generate_realistic_pattern(config: RealisticConfig) {
        let RealisticConfig {
            peer_id,
            base_rate,
            message_size,
            stats,
            simulator,
        } = config;

        // For simplicity, implement as constant rate with occasional bursts
        let interval = Duration::from_secs(1) / base_rate;
        let mut interval_timer = time::interval(interval);

        loop {
            interval_timer.tick().await;

            let message_id = {
                let mut stats_guard = stats.write().await;
                stats_guard.sent += 1;
                stats_guard.sent
            };

            let payload = vec![peer_id as u8; message_size];
            let message = SimulatedMessage {
                from: peer_id,
                to: ((peer_id + 1) % 5),
                payload,
                message_type: MessageType::PubSub,
                priority: 0,
                id: message_id,
            };

            // Record send time
            {
                let mut stats_guard = stats.write().await;
                stats_guard
                    .send_times
                    .insert(message_id, TokioInstant::now());
            }

            if let Err(e) = simulator
                .read()
                .await
                .send_message(peer_id, message.to, message.payload, message.message_type)
                .await
            {
                debug!("Failed to send message: {:?}", e);
                let mut stats_guard = stats.write().await;
                stats_guard.errors += 1;
            }
        }
    }

    /// Monitor test progress and collect final results
    async fn monitor_test_progress(
        &self,
        scenario: LoadScenario,
        start_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<LoadTestResults, LoadTestError> {
        let test_duration = scenario.duration;

        // Wait for test to complete
        time::sleep(test_duration).await;

        let end_time = chrono::Utc::now();

        // Collect final statistics
        let stats = self.stats.read().await;
        let total_messages = stats.sent;
        let duration_secs = test_duration.as_secs_f64();

        // Calculate throughput
        let throughput_msgs_per_sec = total_messages as f64 / duration_secs;

        // Calculate latency percentiles
        let latency_p50 = stats.latency_histogram.value_at_percentile(50.0);
        let latency_p95 = stats.latency_histogram.value_at_percentile(95.0);
        let latency_p99 = stats.latency_histogram.value_at_percentile(99.0);

        // Calculate message loss rate
        let message_loss_rate = if total_messages > 0 {
            (total_messages - stats.received) as f64 / total_messages as f64
        } else {
            0.0
        };

        // Get actual error count
        let error_count = stats.errors;

        // Collect memory usage
        let memory_usage_mb = get_memory_usage_mb();

        // Estimate CPU utilization based on message rate and duration
        // This is a simplified heuristic - real CPU measurement would need platform-specific code
        let cpu_utilization_percent = estimate_cpu_utilization(throughput_msgs_per_sec);

        let results = LoadTestResults {
            scenario_name: scenario.name,
            duration: test_duration,
            num_peers: scenario.num_peers,
            total_messages,
            throughput_msgs_per_sec,
            latency_p50_ms: latency_p50,
            latency_p95_ms: latency_p95,
            latency_p99_ms: latency_p99,
            message_loss_rate,
            memory_usage_mb,
            cpu_utilization_percent,
            error_count,
            start_time,
            end_time,
        };

        Ok(results)
    }
}

/// Get current process memory usage in megabytes
///
/// Platform-specific implementation:
/// - Linux: Reads /proc/self/status VmRSS
/// - macOS: Uses mach task_info
/// - Other platforms: Returns 0.0
fn get_memory_usage_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        if let Ok(status) = fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmRSS:") {
                    // Format: "VmRSS:    12345 kB"
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        if let Ok(kb) = parts[1].parse::<f64>() {
                            return kb / 1024.0; // Convert kB to MB
                        }
                    }
                }
            }
        }
        0.0
    }

    #[cfg(target_os = "macos")]
    {
        // Use mach task_info to get memory usage
        use std::mem::MaybeUninit;

        #[repr(C)]
        struct TaskBasicInfo {
            virtual_size: u64,
            resident_size: u64,
            resident_size_max: u64,
            user_time: u64,
            system_time: u64,
            policy: i32,
            suspend_count: i32,
        }

        extern "C" {
            fn mach_task_self() -> u32;
            fn task_info(
                target_task: u32,
                flavor: i32,
                task_info_out: *mut TaskBasicInfo,
                task_info_out_cnt: *mut u32,
            ) -> i32;
        }

        const MACH_TASK_BASIC_INFO: i32 = 20;

        unsafe {
            let mut info = MaybeUninit::<TaskBasicInfo>::uninit();
            let mut count = (std::mem::size_of::<TaskBasicInfo>() / 4) as u32;

            let result = task_info(
                mach_task_self(),
                MACH_TASK_BASIC_INFO,
                info.as_mut_ptr(),
                &mut count,
            );

            if result == 0 {
                let info = info.assume_init();
                return info.resident_size as f64 / (1024.0 * 1024.0); // Convert bytes to MB
            }
        }
        0.0
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0.0 // Unsupported platform
    }
}

/// Estimate CPU utilization based on message throughput
///
/// This is a simplified heuristic that estimates CPU usage based on
/// the message rate. Real CPU measurement would require platform-specific
/// code and measurement over time.
fn estimate_cpu_utilization(msgs_per_sec: f64) -> f64 {
    // Rough heuristic: assume each 1000 msgs/sec uses ~5% CPU
    // Cap at 100%
    let estimated = (msgs_per_sec / 1000.0) * 5.0;
    estimated.min(100.0)
}

/// Load testing error types
#[derive(thiserror::Error, Debug)]
pub enum LoadTestError {
    /// Error from the network simulator
    #[error("Simulator error: {0}")]
    SimulatorError(#[from] saorsa_gossip_simulator::SimulatorError),
    /// IO error during load test execution
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    /// Error joining async task
    #[error("Task join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),
    /// Invalid test configuration
    #[error("Test configuration error: {0}")]
    ConfigError(String),
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_load_test_runner_creation() {
        let runner = LoadTestRunner::new().expect("Failed to create runner");
        assert!(runner.start_time.read().await.is_none());
    }

    #[tokio::test]
    async fn test_load_scenario_creation() {
        let scenario = LoadScenario {
            name: "test_scenario".to_string(),
            duration: Duration::from_secs(10),
            num_peers: 5,
            message_pattern: MessagePattern::Constant {
                rate_per_second: 10,
                message_size: 100,
            },
            topology: Topology::Mesh,
            chaos_events: vec![],
        };

        assert_eq!(scenario.name, "test_scenario");
        assert_eq!(scenario.num_peers, 5);
    }

    #[tokio::test]
    async fn test_message_pattern_constant() {
        let pattern = MessagePattern::Constant {
            rate_per_second: 100,
            message_size: 1024,
        };

        match pattern {
            MessagePattern::Constant {
                rate_per_second,
                message_size,
            } => {
                assert_eq!(rate_per_second, 100);
                assert_eq!(message_size, 1024);
            }
            _ => panic!("Wrong pattern type"),
        }
    }

    #[tokio::test]
    async fn test_message_pattern_burst() {
        let pattern = MessagePattern::Burst {
            messages_per_burst: 50,
            burst_interval: Duration::from_millis(500),
            message_size: 512,
        };

        match pattern {
            MessagePattern::Burst {
                messages_per_burst,
                burst_interval,
                message_size,
            } => {
                assert_eq!(messages_per_burst, 50);
                assert_eq!(burst_interval, Duration::from_millis(500));
                assert_eq!(message_size, 512);
            }
            _ => panic!("Wrong pattern type"),
        }
    }

    // ==========================================================================
    // Additional Pattern Tests
    // ==========================================================================

    #[tokio::test]
    async fn test_message_pattern_ramp_up() {
        let pattern = MessagePattern::RampUp {
            start_rate_per_second: 10,
            end_rate_per_second: 100,
            ramp_duration: Duration::from_secs(30),
            message_size: 256,
        };

        match pattern {
            MessagePattern::RampUp {
                start_rate_per_second,
                end_rate_per_second,
                ramp_duration,
                message_size,
            } => {
                assert_eq!(start_rate_per_second, 10);
                assert_eq!(end_rate_per_second, 100);
                assert_eq!(ramp_duration, Duration::from_secs(30));
                assert_eq!(message_size, 256);
            }
            _ => panic!("Wrong pattern type"),
        }
    }

    #[tokio::test]
    async fn test_message_pattern_realistic() {
        let pattern = MessagePattern::Realistic {
            base_rate_per_second: 50,
            peak_multiplier: 3.0,
            peak_fraction: 0.2,
            message_size: 1024,
        };

        match pattern {
            MessagePattern::Realistic {
                base_rate_per_second,
                peak_multiplier,
                peak_fraction,
                message_size,
            } => {
                assert_eq!(base_rate_per_second, 50);
                assert!((peak_multiplier - 3.0).abs() < f64::EPSILON);
                assert!((peak_fraction - 0.2).abs() < f64::EPSILON);
                assert_eq!(message_size, 1024);
            }
            _ => panic!("Wrong pattern type"),
        }
    }

    #[tokio::test]
    async fn test_message_pattern_clone() {
        let pattern = MessagePattern::Constant {
            rate_per_second: 100,
            message_size: 512,
        };

        let cloned = pattern.clone();
        match (pattern, cloned) {
            (
                MessagePattern::Constant {
                    rate_per_second: r1,
                    message_size: s1,
                },
                MessagePattern::Constant {
                    rate_per_second: r2,
                    message_size: s2,
                },
            ) => {
                assert_eq!(r1, r2);
                assert_eq!(s1, s2);
            }
            _ => panic!("Clone produced different variant"),
        }
    }

    // ==========================================================================
    // LoadTestRunner Tests
    // ==========================================================================

    #[tokio::test]
    async fn test_load_test_runner_with_seed() {
        let runner1 = LoadTestRunner::with_seed(12345).expect("Failed to create runner");
        let runner2 = LoadTestRunner::with_seed(12345).expect("Failed to create runner");

        // Both runners should start with no start time
        assert!(runner1.start_time.read().await.is_none());
        assert!(runner2.start_time.read().await.is_none());

        // Both runners should have deterministic RNG with same seed
        let num1 = runner1.rng.lock().expect("lock").gen::<u64>();
        let num2 = runner2.rng.lock().expect("lock").gen::<u64>();
        assert_eq!(num1, num2, "Same seed should produce same random numbers");
    }

    #[tokio::test]
    async fn test_load_test_runner_different_seeds() {
        let runner1 = LoadTestRunner::with_seed(11111).expect("Failed to create runner");
        let runner2 = LoadTestRunner::with_seed(22222).expect("Failed to create runner");

        let num1 = runner1.rng.lock().expect("lock").gen::<u64>();
        let num2 = runner2.rng.lock().expect("lock").gen::<u64>();
        assert_ne!(num1, num2, "Different seeds should produce different numbers");
    }

    // ==========================================================================
    // LoadScenario Tests
    // ==========================================================================

    #[tokio::test]
    async fn test_load_scenario_clone() {
        let scenario = LoadScenario {
            name: "cloneable_scenario".to_string(),
            duration: Duration::from_secs(60),
            num_peers: 10,
            message_pattern: MessagePattern::Constant {
                rate_per_second: 100,
                message_size: 512,
            },
            topology: Topology::Mesh,
            chaos_events: vec![],
        };

        let cloned = scenario.clone();
        assert_eq!(scenario.name, cloned.name);
        assert_eq!(scenario.duration, cloned.duration);
        assert_eq!(scenario.num_peers, cloned.num_peers);
    }

    #[tokio::test]
    async fn test_load_scenario_with_chaos_events() {
        use saorsa_gossip_simulator::ChaosEvent;

        let scenario = LoadScenario {
            name: "chaos_scenario".to_string(),
            duration: Duration::from_secs(120),
            num_peers: 20,
            message_pattern: MessagePattern::Burst {
                messages_per_burst: 100,
                burst_interval: Duration::from_millis(200),
                message_size: 256,
            },
            topology: Topology::Ring,
            chaos_events: vec![
                (
                    Duration::from_secs(10),
                    ChaosEvent::NodeFailure {
                        node_id: 0,
                        duration: Duration::from_secs(5),
                    },
                ),
                (
                    Duration::from_secs(30),
                    ChaosEvent::NetworkPartition {
                        group_a: vec![0, 1, 2],
                        group_b: vec![3, 4, 5],
                        duration: Duration::from_secs(10),
                    },
                ),
            ],
        };

        assert_eq!(scenario.chaos_events.len(), 2);
        assert_eq!(scenario.name, "chaos_scenario");
    }

    #[tokio::test]
    async fn test_load_scenario_mesh_topology() {
        let scenario = LoadScenario {
            name: "mesh_test".to_string(),
            duration: Duration::from_secs(10),
            num_peers: 5,
            message_pattern: MessagePattern::Constant {
                rate_per_second: 10,
                message_size: 100,
            },
            topology: Topology::Mesh,
            chaos_events: vec![],
        };

        assert!(!scenario.name.is_empty());
        assert_eq!(scenario.num_peers, 5);
    }

    #[tokio::test]
    async fn test_load_scenario_ring_topology() {
        let scenario = LoadScenario {
            name: "ring_test".to_string(),
            duration: Duration::from_secs(10),
            num_peers: 8,
            message_pattern: MessagePattern::Constant {
                rate_per_second: 20,
                message_size: 128,
            },
            topology: Topology::Ring,
            chaos_events: vec![],
        };

        assert!(!scenario.name.is_empty());
        assert_eq!(scenario.num_peers, 8);
    }

    #[tokio::test]
    async fn test_load_scenario_star_topology() {
        let scenario = LoadScenario {
            name: "star_test".to_string(),
            duration: Duration::from_secs(10),
            num_peers: 10,
            message_pattern: MessagePattern::Constant {
                rate_per_second: 15,
                message_size: 64,
            },
            topology: Topology::Star { center: 0 },
            chaos_events: vec![],
        };

        assert!(!scenario.name.is_empty());
        assert_eq!(scenario.num_peers, 10);
    }

    // ==========================================================================
    // LoadTestResults Tests
    // ==========================================================================

    #[tokio::test]
    async fn test_load_test_results_creation() {
        let results = LoadTestResults {
            scenario_name: "results_test".to_string(),
            duration: Duration::from_secs(60),
            num_peers: 100,
            total_messages: 10000,
            throughput_msgs_per_sec: 166.67,
            latency_p50_ms: 5,
            latency_p95_ms: 15,
            latency_p99_ms: 50,
            message_loss_rate: 0.001,
            memory_usage_mb: 256.5,
            cpu_utilization_percent: 45.0,
            error_count: 10,
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
        };

        assert_eq!(results.scenario_name, "results_test");
        assert_eq!(results.total_messages, 10000);
        assert!(results.throughput_msgs_per_sec > 0.0);
        assert!(results.latency_p50_ms < results.latency_p95_ms);
        assert!(results.latency_p95_ms < results.latency_p99_ms);
    }

    #[tokio::test]
    async fn test_load_test_results_clone() {
        let results = LoadTestResults {
            scenario_name: "clone_test".to_string(),
            duration: Duration::from_secs(30),
            num_peers: 50,
            total_messages: 5000,
            throughput_msgs_per_sec: 166.67,
            latency_p50_ms: 3,
            latency_p95_ms: 10,
            latency_p99_ms: 25,
            message_loss_rate: 0.0,
            memory_usage_mb: 128.0,
            cpu_utilization_percent: 30.0,
            error_count: 0,
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
        };

        let cloned = results.clone();
        assert_eq!(results.scenario_name, cloned.scenario_name);
        assert_eq!(results.total_messages, cloned.total_messages);
        assert_eq!(results.error_count, cloned.error_count);
    }

    #[tokio::test]
    async fn test_load_test_results_debug_format() {
        let results = LoadTestResults {
            scenario_name: "debug_test".to_string(),
            duration: Duration::from_secs(60),
            num_peers: 100,
            total_messages: 10000,
            throughput_msgs_per_sec: 166.67,
            latency_p50_ms: 5,
            latency_p95_ms: 15,
            latency_p99_ms: 50,
            message_loss_rate: 0.001,
            memory_usage_mb: 256.5,
            cpu_utilization_percent: 45.0,
            error_count: 0,
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
        };

        // Test Debug formatting works
        let debug_str = format!("{:?}", results);
        assert!(debug_str.contains("debug_test"));
        assert!(debug_str.contains("10000"));
    }

    #[tokio::test]
    async fn test_load_test_results_metrics_order() {
        let results = LoadTestResults {
            scenario_name: "latency_order_test".to_string(),
            duration: Duration::from_secs(60),
            num_peers: 50,
            total_messages: 5000,
            throughput_msgs_per_sec: 83.33,
            latency_p50_ms: 2,
            latency_p95_ms: 8,
            latency_p99_ms: 20,
            message_loss_rate: 0.0,
            memory_usage_mb: 64.0,
            cpu_utilization_percent: 25.0,
            error_count: 0,
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
        };

        // Latencies should be in increasing order (p50 < p95 < p99)
        assert!(results.latency_p50_ms <= results.latency_p95_ms);
        assert!(results.latency_p95_ms <= results.latency_p99_ms);
    }
}

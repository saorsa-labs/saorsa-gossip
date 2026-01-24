#![warn(missing_docs)]

//! High-level runtime helpers for Saorsa Gossip.
//!
//! This crate assembles the lower level Saorsa crates (transport, membership,
//! pubsub, presence, coordinator, rendezvous) into a composable runtime that
//! downstream applications can embed without re-implementing the wiring logic.
//!
//! # Getting Started
//!
//! The easiest way to create a gossip runtime is using [`GossipContext`]:
//!
//! ```ignore
//! use saorsa_gossip_runtime::GossipContext;
//!
//! let runtime = GossipContext::with_defaults()
//!     .with_bind_addr("0.0.0.0:10000".parse()?)
//!     .build()
//!     .await?;
//! ```
//!
//! For advanced configuration, use [`GossipRuntimeBuilder`] directly.

mod context;
mod coordinator;
mod rendezvous;
mod runtime;

pub use context::GossipContext;
pub use coordinator::CoordinatorClient;
pub use rendezvous::RendezvousClient;
pub use runtime::{GossipRuntime, GossipRuntimeBuilder, GossipRuntimeConfig};

// Re-export commonly used transport types for convenience
pub use saorsa_gossip_transport::UdpTransportAdapterConfig;

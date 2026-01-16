#![warn(missing_docs)]

//! High-level runtime helpers for Saorsa Gossip.
//!
//! This crate assembles the lower level Saorsa crates (transport, membership,
//! pubsub, presence, coordinator, rendezvous) into a composable runtime that
//! downstream applications can embed without re-implementing the wiring logic.

mod coordinator;
mod rendezvous;
mod runtime;

pub use coordinator::CoordinatorClient;
pub use rendezvous::RendezvousClient;
pub use runtime::{GossipRuntime, GossipRuntimeBuilder, GossipRuntimeConfig};

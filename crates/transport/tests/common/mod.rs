//! Shared fixtures for transport integration tests.
//!
//! Thin re-export of `saorsa_gossip_transport::testing` so the helpers live
//! in one place and downstream crates (pubsub, membership, runtime) can
//! consume the same fixture surface via the `test-helpers` feature.
#![allow(unused_imports, dead_code)]

pub use saorsa_gossip_transport::testing::{
    connected_pair, init_tracing, loopback_from, loopback_star, wait_until_connected,
};

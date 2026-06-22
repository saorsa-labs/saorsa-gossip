//! Canonical facade for the Saorsa Gossip protocol stack.
//!
//! `saorsa-gossip` is the front-door crate for applications using the Saorsa
//! Gossip overlay. It re-exports the published component crates under stable,
//! predictable module names while keeping the lower-level crates available for
//! advanced users who want finer dependency control.
//!
//! The package also ships the experimental `saorsa-gossip` CLI binary for
//! exercising the stack. The library facade is the primary API surface for new
//! Rust integrations.

/// Core identifiers, message headers, wire formats, admission and peer-health types.
pub mod types {
    pub use saorsa_gossip_types::*;
}

/// Post-quantum peer identity and signing helpers.
pub mod identity {
    pub use saorsa_gossip_identity::*;
}

/// Transport traits and QUIC/ant-quic integration.
pub mod transport {
    pub use saorsa_gossip_transport::*;
}

/// Membership and failure-detection layer.
pub mod membership {
    pub use saorsa_gossip_membership::*;
}

/// Pub/sub gossip dissemination.
pub mod pubsub {
    pub use saorsa_gossip_pubsub::*;
}

/// Group-security helpers.
pub mod groups {
    pub use saorsa_gossip_groups::*;
}

/// Presence beacons and online-state helpers.
pub mod presence {
    pub use saorsa_gossip_presence::*;
}

/// Delta-CRDT synchronisation helpers.
pub mod crdt {
    pub use saorsa_gossip_crdt_sync::*;
}

/// Coordinator adverts and bootstrap helpers.
pub mod coordinator {
    pub use saorsa_gossip_coordinator::*;
}

/// Rendezvous sharding and provider summaries.
pub mod rendezvous {
    pub use saorsa_gossip_rendezvous::*;
}

/// High-level runtime assembly API.
pub mod runtime {
    pub use saorsa_gossip_runtime::*;
}

/// Common imports for application developers.
pub mod prelude {
    pub use crate::runtime::*;
    pub use crate::types::*;
}

// Keep root-level exports deliberately small: the high-level runtime and core
// types are the normal starting points. Lower-level APIs remain namespaced.
pub use saorsa_gossip_runtime::*;
pub use saorsa_gossip_types::*;

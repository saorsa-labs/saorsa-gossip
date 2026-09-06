//! Repository-only access to the authentic published legacy implementations.
//!
//! Keep these pins out of publishable manifests: Cargo cannot resolve exact
//! 0.5.66 and compatible 0.5.76 requirements from the same registry together.

pub use legacy_identity;
pub use legacy_pubsub;
pub use legacy_transport;
pub use legacy_types;

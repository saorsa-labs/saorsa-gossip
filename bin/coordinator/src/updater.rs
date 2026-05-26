//! Self-update functionality for Saorsa Gossip Coordinator
//!
//! Provides automatic update checking and installation from GitHub releases.

use anyhow::{Context, Result};
use std::time::Duration;

const REPO_OWNER: &str = "saorsa-labs";
const REPO_NAME: &str = "saorsa-gossip";
const BIN_NAME: &str = "saorsa-gossip-coordinator";

/// Check for updates and return the latest version if newer than current
pub async fn check_for_update() -> Result<Option<String>> {
    let current_version = env!("CARGO_PKG_VERSION");

    tracing::debug!("Current version: {}", current_version);
    tracing::debug!("Checking for updates from GitHub releases...");

    match self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(current_version)
        .build()
    {
        Ok(updater) => match updater.get_latest_release() {
            Ok(release) => {
                if release.version.as_str() > current_version {
                    tracing::info!(
                        "New version available: {} (current: {})",
                        release.version,
                        current_version
                    );
                    Ok(Some(release.version))
                } else {
                    tracing::debug!("Already on latest version: {}", current_version);
                    Ok(None)
                }
            }
            Err(e) => {
                tracing::warn!("Failed to check for updates: {}", e);
                Ok(None)
            }
        },
        Err(e) => {
            tracing::warn!("Failed to build updater: {}", e);
            Ok(None)
        }
    }
}

/// Perform the update to the latest version
#[allow(dead_code)]
pub async fn perform_update() -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    tracing::info!("Checking for updates...");
    tracing::info!("Current version: {}", current_version);

    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(current_version)
        .build()
        .context("Failed to build updater")?
        .update()
        .context("Failed to perform update")?;

    match status {
        self_update::Status::UpToDate(version) => {
            tracing::info!("Already up to date (version: {})", version);
        }
        self_update::Status::Updated(version) => {
            tracing::info!("Successfully updated to version: {}", version);
            tracing::warn!("Please restart the coordinator to use the new version.");
        }
    }

    Ok(())
}

/// Background update checker that checks every 6 hours
pub fn start_background_checker() {
    tokio::spawn(async {
        let check_interval = Duration::from_secs(6 * 60 * 60); // 6 hours

        loop {
            tokio::time::sleep(check_interval).await;

            if let Ok(Some(new_version)) = check_for_update().await {
                tracing::warn!(
                    "⚠️  Update available: {} - please restart with the new version",
                    new_version
                );
            }
        }
    });
}

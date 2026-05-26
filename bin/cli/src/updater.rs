//! Self-update functionality for Saorsa Gossip CLI
//!
//! Provides automatic update checking and installation from GitHub releases.

use anyhow::{Context, Result};
use std::time::Duration;

const REPO_OWNER: &str = "saorsa-labs";
const REPO_NAME: &str = "saorsa-gossip";
const BIN_NAME: &str = "saorsa-gossip";

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
pub async fn perform_update() -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    println!("🔍 Checking for updates...");
    println!("   Current version: {}", current_version);

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
            println!("✓ Already up to date (version: {})", version);
        }
        self_update::Status::Updated(version) => {
            println!("✓ Successfully updated to version: {}", version);
            println!("  Please restart the application to use the new version.");
        }
    }

    Ok(())
}

/// Background update checker that checks every 6 hours
#[allow(dead_code)]
pub async fn start_background_checker() {
    tokio::spawn(async {
        let check_interval = Duration::from_secs(6 * 60 * 60); // 6 hours

        loop {
            tokio::time::sleep(check_interval).await;

            if let Ok(Some(new_version)) = check_for_update().await {
                tracing::info!(
                    "Update available: {} - run 'saorsa-gossip update' to upgrade",
                    new_version
                );
                println!(
                    "\n🔔 Update available: {} - run 'saorsa-gossip update' to upgrade\n",
                    new_version
                );
            }
        }
    });
}

/// Check if we should show update notification (rate-limited)
pub async fn should_check_update(config_dir: &std::path::Path) -> bool {
    let last_check_file = config_dir.join(".last_update_check");

    if let Ok(metadata) = tokio::fs::metadata(&last_check_file).await {
        if let Ok(modified) = metadata.modified() {
            if let Ok(elapsed) = modified.elapsed() {
                // Only check every 6 hours
                if elapsed < Duration::from_secs(6 * 60 * 60) {
                    return false;
                }
            }
        }
    }

    // Update last check time
    tokio::fs::write(&last_check_file, b"").await.is_ok()
}

/// Perform a silent update check and notify if available (non-blocking)
pub async fn silent_update_check(config_dir: &std::path::Path) {
    if !should_check_update(config_dir).await {
        return;
    }

    if let Ok(Some(new_version)) = check_for_update().await {
        println!(
            "\n🔔 Update available: {} - run 'saorsa-gossip update' to upgrade\n",
            new_version
        );
    }
}

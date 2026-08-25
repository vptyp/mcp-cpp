//! Common utilities for MCP tools

use crate::project::ComponentSession;
use crate::project::index::IndexStatusView;
use std::time::Duration;
use tracing::info;

/// Helper function to serialize JSON content and handle errors gracefully
pub fn serialize_result(content: &serde_json::Value) -> String {
    serde_json::to_string_pretty(content)
        .unwrap_or_else(|e| format!("Error serializing result: {e}"))
}

/// Selective indexing wait logic for MCP tools
///
/// This function implements the common pattern where:
/// - Document-specific operations (with hints) skip indexing wait and return current status
/// - Workspace operations wait for indexing completion based on timeout
///
/// # Arguments
/// * `component_session` - The component session to use for indexing operations
/// * `skip_indexing_condition` - Whether to skip indexing wait (e.g., has location_hint or files)
/// * `wait_timeout` - Optional timeout in seconds (falls back to the session's configured default)
/// * `operation_type` - Human-readable operation type for logging (e.g., "document search", "workspace analysis")
///
/// # Returns
/// * `Some(IndexStatusView)` - Current index status (either on skip or timeout)
/// * `None` - Indexing completed successfully
pub async fn handle_selective_indexing_wait(
    component_session: &ComponentSession,
    skip_indexing_condition: bool,
    wait_timeout: Option<u64>,
    operation_type: &str,
) -> Option<IndexStatusView> {
    if skip_indexing_condition {
        // Document-specific operation: Skip indexing wait and return current status
        info!("{} detected - skipping indexing wait", operation_type);
        Some(component_session.get_index_status().await)
    } else {
        // Workspace operation: Wait for indexing based on timeout parameter
        // No explicit timeout means "use whatever this project configured",
        // not a compiled-in constant -- large trees legitimately need minutes.
        let timeout = match wait_timeout {
            Some(secs) => Duration::from_secs(secs),
            None => component_session.index_wait_timeout(),
        };

        if timeout.is_zero() {
            info!("Zero timeout specified - skipping indexing wait");
            Some(component_session.get_index_status().await)
        } else {
            info!(
                "{} detected - waiting for indexing completion ({}s)",
                operation_type,
                timeout.as_secs()
            );
            match component_session.ensure_indexed(timeout).await {
                Ok(()) => {
                    info!("Indexing completed successfully");
                    None // No need to include status on success
                }
                Err(e) => {
                    info!("Indexing timeout or failure: {} - including status", e);
                    Some(component_session.get_index_status().await)
                }
            }
        }
    }
}

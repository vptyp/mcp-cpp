//! Common utilities for MCP tools

use std::time::Duration;

use crate::clangd::progress::IndexStatus;
use crate::project::ComponentSession;

/// Helper function to serialize JSON content and handle errors gracefully
pub fn serialize_result(content: &serde_json::Value) -> String {
    serde_json::to_string_pretty(content)
        .unwrap_or_else(|e| format!("Error serializing result: {e}"))
}

/// Wait only on clangd's standard progress notifications. The result is
/// returned when there is still useful status information to report.
pub async fn wait_for_clangd_index(
    component_session: &ComponentSession,
    skip_wait: bool,
    wait_timeout: Option<u64>,
) -> Option<IndexStatus> {
    if skip_wait {
        return Some(component_session.index_status().await);
    }
    let timeout = wait_timeout
        .map(Duration::from_secs)
        .unwrap_or_else(|| component_session.index_wait_timeout());
    if timeout.is_zero() {
        return Some(component_session.index_status().await);
    }
    let status = component_session.wait_for_index_status(timeout).await;
    (status.state != "Completed").then_some(status)
}

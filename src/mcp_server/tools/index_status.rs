//! Current index status retrieval
//!
//! Provides a lightweight tool that returns only the current clangd indexing
//! status for a build directory (in progress / percentage / files indexed /
//! ETA) without running an expensive search. Useful for monitoring a long
//! background index build.

use rust_mcp_sdk::macros::{JsonSchema, mcp_tool};
use rust_mcp_sdk::schema::{CallToolResult, TextContent, schema_utils::CallToolError};
use std::sync::Arc;
use tracing::instrument;

use crate::mcp_server::tools::utils;
use crate::project::index::IndexStatusView;
use crate::project::{ComponentSession, ProjectComponent};

/// Result structure for the get_index_status tool
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IndexStatusResult {
    pub success: bool,
    pub build_directory: String,
    pub index_status: IndexStatusView,
}

#[mcp_tool(
    name = "get_index_status",
    description = "Return the current clangd indexing status for a build directory without running a search. \
                   Reports whether background indexing is in progress, the progress percentage, files indexed vs \
                   total, and an estimated time remaining. Lightweight and fast — use it to monitor a long \
                   background index build (e.g. a large Chromium tree).

                   INPUT PARAMETERS:
                   • build_directory: Custom build directory path (STRONGLY PREFER ABSOLUTE PATHS from get_project_details)
                   • wait_timeout: Optional seconds to wait for indexing to complete before returning the status \
                     (default: 0 = return the current status immediately)"
)]
#[derive(Debug, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct GetIndexStatusTool {
    /// Build directory path containing compile_commands.json. STRONGLY RECOMMENDED: Use absolute paths from get_project_details output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_directory: Option<String>,

    /// Optional seconds to wait for indexing to complete (default: 0 = immediate current status)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<u64>,
}

impl GetIndexStatusTool {
    #[instrument(name = "get_index_status", skip(self, component_session, component))]
    pub async fn call_tool(
        &self,
        component_session: Arc<ComponentSession>,
        component: &ProjectComponent,
    ) -> Result<CallToolResult, CallToolError> {
        // If the caller asked us to wait for completion, do a bounded wait; otherwise
        // just snapshot the current status immediately.
        if let Some(wait_timeout) = self.wait_timeout.filter(|t| *t > 0) {
            let _ = utils::handle_selective_indexing_wait(
                &component_session,
                false,
                Some(wait_timeout),
                "get_index_status",
            )
            .await;
        }

        let index_status = component_session.get_index_status().await;

        let result = IndexStatusResult {
            success: true,
            build_directory: component.build_dir_path.to_string_lossy().to_string(),
            index_status,
        };

        let json = serde_json::to_string(&result).map_err(|e| {
            CallToolError::new(std::io::Error::other(format!(
                "Failed to serialize index status: {}",
                e
            )))
        })?;

        Ok(CallToolResult::text_content(vec![TextContent::from(json)]))
    }
}

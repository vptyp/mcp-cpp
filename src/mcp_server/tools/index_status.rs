//! Exposes clangd's own background-index progress over MCP.

use rust_mcp_sdk::macros::{JsonSchema, mcp_tool};
use rust_mcp_sdk::schema::{CallToolResult, TextContent, schema_utils::CallToolError};
use std::sync::Arc;
use std::time::Duration;

use crate::clangd::progress::IndexStatus;
use crate::project::{ComponentSession, ProjectComponent};

#[derive(Debug, serde::Serialize)]
pub struct IndexStatusResult {
    pub success: bool,
    pub build_directory: String,
    pub index_status: IndexStatus,
}

#[mcp_tool(
    name = "get_index_status",
    description = "Return the latest background-index status reported directly by clangd. No cache files or clangd logs are read."
)]
#[derive(Debug, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct GetIndexStatusTool {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_directory: Option<String>,
    /// Optionally wait for clangd's current indexing pass to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<u64>,
}

impl GetIndexStatusTool {
    pub async fn call_tool(
        &self,
        component_session: Arc<ComponentSession>,
        component: &ProjectComponent,
    ) -> Result<CallToolResult, CallToolError> {
        let index_status = match self.wait_timeout.filter(|timeout| *timeout > 0) {
            Some(timeout) => {
                component_session
                    .wait_for_index_status(Duration::from_secs(timeout))
                    .await
            }
            None => component_session.index_status().await,
        };
        let result = IndexStatusResult {
            success: true,
            build_directory: component.build_dir_path.display().to_string(),
            index_status,
        };
        let json = serde_json::to_string(&result)
            .map_err(|e| CallToolError::new(std::io::Error::other(e)))?;
        Ok(CallToolResult::text_content(vec![TextContent::from(json)]))
    }
}

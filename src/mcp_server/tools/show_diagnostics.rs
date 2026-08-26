//! File diagnostics retrieval functionality
//!
//! This module provides a tool that returns clangd's diagnostics (compile errors,
//! warnings, notes) for a specific source file. It leverages the LSP
//! `textDocument/publishDiagnostics` push mechanism captured by the
//! `DiagnosticsCollector`.

use rust_mcp_sdk::macros::{JsonSchema, mcp_tool};
use rust_mcp_sdk::schema::{CallToolResult, TextContent, schema_utils::CallToolError};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, instrument, warn};

use crate::clangd::progress::IndexStatus;
use crate::project::{ComponentSession, ProjectComponent};

/// Default timeout (seconds) to wait for clangd to publish diagnostics.
const DEFAULT_DIAGNOSTICS_TIMEOUT_SECS: u64 = 20;

/// Result structure for the show_diagnostics tool
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticsResult {
    pub success: bool,
    pub file: String,
    pub build_directory: String,
    pub total: usize,
    /// Number of diagnostics with severity `Error`
    pub errors: usize,
    /// Number of diagnostics with severity `Warning`
    pub warnings: usize,
    /// Number of informational/hint diagnostics
    pub notes: usize,
    /// Whether we gave up waiting because clangd didn't publish in time
    pub timed_out: bool,
    pub diagnostics: Vec<lsp_types::Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_status: Option<IndexStatus>,
}

#[mcp_tool(
    name = "show_diagnostics",
    description = "Retrieve semantic diagnostics (compile errors, warnings, notes) for a single C++ source file \
                   using clangd. Diagnostics are only available after a file has been parsed by clangd, which \
                   happens automatically when the file is opened for this analysis.

                   🚀 RECOMMENDED WORKFLOW FOR AI AGENTS:
                   1. ALWAYS call get_project_details first to discover available build directories
                   2. Use the ABSOLUTE build directory paths from get_project_details output
                   3. Then call show_diagnostics with the file path and build_directory parameter

                   Example workflow:
                   • get_project_details {} → Returns: {\"/home/project/build-debug\": {...}}
                   • show_diagnostics {\"file\": \"/home/project/src/main.cpp\", \"build_directory\": \"/home/project/build-debug\"}

                   📋 WHAT THIS TOOL DOES:
                   • Opens the target file in the clangd LSP session (opening triggers parsing)
                   • Captures the textDocument/publishDiagnostics notification clangd sends
                   • Returns errors, warnings, and info/hint diagnostics with their exact source ranges
                   • Each diagnostic includes: severity, message, range (line/character), and optional related info

                   💡 TYPICAL USE CASES:
                   • Verify a file compiles cleanly before committing changes
                   • Find the exact compile errors clangd reports for a broken file
                   • Understand warnings that may indicate subtle bugs or UB
                   • Locate the precise line/column of an error via the diagnostic range

                   INPUT PARAMETERS:
                   • file: Path to the C++ source file to analyze (relative paths resolved against the project root)
                   • build_directory: Custom build directory path (STRONGLY PREFER ABSOLUTE PATHS from get_project_details)
                   • wait_timeout: Timeout in seconds to wait for diagnostics (default: 20s, 0 = no wait)"
)]
#[derive(Debug, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct ShowDiagnosticsTool {
    /// Path to the C++ source file to check for diagnostics. Supports absolute paths or
    /// paths relative to the project root (e.g. "src/main.cpp").
    pub file: String,

    /// Build directory path containing compile_commands.json. STRONGLY RECOMMENDED: Use absolute paths from get_project_details output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_directory: Option<String>,

    /// Timeout in seconds to wait for clangd to publish diagnostics (default: 20s, 0 = no wait)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<u64>,
}

impl ShowDiagnosticsTool {
    #[instrument(name = "show_diagnostics", skip(self, component_session, component))]
    pub async fn call_tool(
        &self,
        component_session: Arc<ComponentSession>,
        component: &ProjectComponent,
    ) -> Result<CallToolResult, CallToolError> {
        let wait_timeout = self
            .wait_timeout
            .unwrap_or(DEFAULT_DIAGNOSTICS_TIMEOUT_SECS);

        info!(
            "Collecting diagnostics: file='{}', wait_timeout={}s",
            self.file, wait_timeout
        );

        let index_status = component_session.index_status().await;

        // Resolve the file path to an absolute path using the project root
        let project_root = &component.source_root_path;
        let absolute_path = if std::path::Path::new(&self.file).is_absolute() {
            std::path::PathBuf::from(&self.file)
        } else {
            let resolved = project_root.join(&self.file);
            if !resolved.exists() {
                return Err(CallToolError::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "File not found: {} (resolved to {})",
                        self.file,
                        resolved.display()
                    ),
                )));
            }
            resolved
        };

        // If a zero/negative timeout was specified, only return cached diagnostics
        // (won't wait for a fresh publish).
        let timeout = Duration::from_secs(wait_timeout);
        let diagnostics = component_session
            .get_file_diagnostics(&absolute_path, timeout)
            .await
            .map_err(|e| {
                CallToolError::new(std::io::Error::other(format!(
                    "Failed to collect diagnostics: {}",
                    e
                )))
            })?;

        let (diagnostics, timed_out) = match diagnostics {
            Some(diags) => (diags, false),
            None => {
                warn!(
                    "Timed out waiting for diagnostics for {}",
                    absolute_path.display()
                );
                (Vec::new(), true)
            }
        };

        // Count by severity
        let errors = diagnostics
            .iter()
            .filter(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
            .count();
        let warnings = diagnostics
            .iter()
            .filter(|d| d.severity == Some(lsp_types::DiagnosticSeverity::WARNING))
            .count();
        let notes = diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d.severity,
                    Some(lsp_types::DiagnosticSeverity::INFORMATION)
                        | Some(lsp_types::DiagnosticSeverity::HINT)
                )
            })
            .count();

        let result = DiagnosticsResult {
            success: true,
            file: absolute_path.to_string_lossy().to_string(),
            build_directory: component.build_dir_path.display().to_string(),
            total: diagnostics.len(),
            errors,
            warnings,
            notes,
            timed_out,
            diagnostics,
            index_status: Some(index_status),
        };

        let output = serde_json::to_string_pretty(&result).map_err(|e| {
            CallToolError::new(std::io::Error::other(format!(
                "Failed to serialize result: {}",
                e
            )))
        })?;

        Ok(CallToolResult::text_content(vec![TextContent::from(
            output,
        )]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_show_diagnostics_minimal() {
        let data = json!({ "file": "src/main.cpp" });
        let tool: ShowDiagnosticsTool = serde_json::from_value(data).unwrap();
        assert_eq!(tool.file, "src/main.cpp");
        assert_eq!(tool.build_directory, None);
        assert_eq!(tool.wait_timeout, None);
    }

    #[test]
    fn test_show_diagnostics_full() {
        let data = json!({
            "file": "/abs/path/main.cpp",
            "build_directory": "/abs/path/build",
            "wait_timeout": 5
        });
        let tool: ShowDiagnosticsTool = serde_json::from_value(data).unwrap();
        assert_eq!(tool.file, "/abs/path/main.cpp");
        assert_eq!(tool.build_directory, Some("/abs/path/build".to_string()));
        assert_eq!(tool.wait_timeout, Some(5));
    }

    #[test]
    fn test_show_diagnostics_missing_file_fails() {
        let data = json!({});
        assert!(serde_json::from_value::<ShowDiagnosticsTool>(data).is_err());
    }

    #[test]
    fn test_show_diagnostics_serialize_result() {
        let diag = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 4,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            code: None,
            code_description: None,
            source: Some("clangd".to_string()),
            message: "unused variable".to_string(),
            related_information: None,
            tags: None,
            data: None,
        };
        let result = DiagnosticsResult {
            success: true,
            file: "main.cpp".to_string(),
            build_directory: "/build".to_string(),
            total: 1,
            errors: 0,
            warnings: 1,
            notes: 0,
            timed_out: false,
            diagnostics: vec![diag],
            index_status: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["total"], 1);
        assert_eq!(json["warnings"], 1);
        assert_eq!(json["diagnostics"][0]["message"], "unused variable");
    }
}

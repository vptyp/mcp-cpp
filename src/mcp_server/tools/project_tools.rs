//! Project analysis tools for multi-provider build systems

use rust_mcp_sdk::macros::{JsonSchema, mcp_tool};
use rust_mcp_sdk::schema::{CallToolResult, TextContent, schema_utils::CallToolError};
use tracing::{info, instrument};

use super::utils::serialize_result;
use crate::config::AppConfig;
use crate::project::ProjectWorkspace;

#[mcp_tool(
    name = "get_project_details",
    description = "Get comprehensive project analysis including build configurations, components, \
                   and global compilation database information. Provides complete workspace \
                   intelligence for multi-provider build systems.

                   🚀 PRIMARY PURPOSE FOR AI AGENTS:
                   This tool provides ABSOLUTE BUILD DIRECTORY PATHS that you should use with search_symbols and analyze_symbol_context.
                   ALWAYS call this tool FIRST to discover available build directories before using other MCP tools.

                   TYPICAL AI AGENT WORKFLOW:
                   1. get_project_details {} → Get absolute build paths: {\"/home/project/build-debug\": {...}}
                   2. search_symbols {\"query\": \"\", \"build_directory\": \"/home/project/build-debug\"} → Explore symbols
                   3. analyze_symbol_context {\"symbol\": \"FoundSymbol\", \"build_directory\": \"/home/project/build-debug\"}

                   🏗️ PROJECT OVERVIEW:
                   • Project name and root directory information
                   • Global compilation database path (if configured)
                   • Component count and provider type summary
                   • Discovery timestamp and scan configuration
                   • ABSOLUTE BUILD DIRECTORY PATHS for use in other tools

                   🔧 MULTI-PROVIDER DISCOVERY:
                   • Automatic detection of CMake projects (CMakeLists.txt + build directories)
                   • Meson project support (meson.build + build configurations)
                   • Extensible architecture ready for Bazel, Buck, xmake, and other build systems
                   • Unified component representation across all providers

                   ⚙️ BUILD CONFIGURATION ANALYSIS:
                   • Generator type identification (Ninja, Unix Makefiles, Visual Studio, etc.)
                   • Build type classification (Debug, Release, RelWithDebInfo, MinSizeRel)
                   • Compiler toolchain detection and version information
                   • Build options and feature flags extraction

                   📋 COMPILATION DATABASE STATUS:
                   • Global compilation database path (overrides component-specific databases)
                   • Per-component compile_commands.json availability and validity
                   • LSP server compatibility assessment across all build systems

                   🎯 PROJECT STRUCTURE DETAILS:
                   Each discovered component includes:
                   • Build directory path (ABSOLUTE) and source root location
                   • Provider type (cmake, meson, etc.) for build system identification
                   • Generator and build type information in standardized format
                   • Complete build options and configuration details
                   • Compilation database status for LSP integration

                   🎯 PRIMARY USE CASES:
                   Project assessment • Build system inventory • LSP setup validation
                   • Development environment verification • CI/CD build matrix generation
                   • Global compilation database configuration analysis
                   • PROVIDING ABSOLUTE PATHS for other MCP tools

                   INPUT PARAMETERS:
                   • path (optional): Project root path to scan (triggers fresh scan if different) - AVOID \".\" use None for cached scan
                   • depth (optional): Scan depth for component discovery (0-10 levels) - only specify if different from cached scan
                   • include_details (optional): Include detailed build options (default: false)

                   ⚙️ EFFECTIVE CONFIGURATION:
                   • The `configuration` block reports the settings the server actually resolved
                     (clangd path and arguments, timeouts, scan depth, HTTP bind address) and the
                     `.mcp-cpp.yaml` file they came from, or null when none was found.
                   • Use it to confirm a config file was picked up before debugging clangd behaviour

                   OUTPUT MODES:
                   • Short view (default): Essential info + build_options_count to prevent context exhaustion
                   • Detailed view (include_details=true): All build options for debugging/analysis

                   RECOMMENDED USAGE:
                   • Use default for project overview and general development
                   • Use include_details=true only for build configuration debugging
                   • Copy the absolute build directory paths from output to use in other tools"
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema)]
pub struct GetProjectDetailsTool {
    /// Optional project root path to scan. DEFAULT: uses server's cached scan results.
    ///
    /// IMPORTANT: AVOID using "." as it discards cached scan and may override user choices.
    /// Use None (omit parameter) to use cached scan results which is usually what you want.
    ///
    /// FORMATS ACCEPTED:
    /// • Relative path: "..", "subproject/" (AVOID "." - use None instead)
    /// • Absolute path: "/home/project", "/path/to/workspace"
    ///
    /// BEHAVIOR: When specified and different from server's initial scan, performs
    /// a fresh scan without caching the results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Scan depth for project component discovery. DEFAULT: uses server's initial scan depth.
    ///
    /// RANGE: 0-10 levels (0 = only root directory, 3 = reasonable default)
    ///
    /// BEHAVIOR: When specified and different from server's initial scan, performs
    /// a fresh scan without caching the results. Higher depths may take longer
    /// but discover components in deeply nested directory structures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,

    /// Include detailed build options and configuration variables. DEFAULT: false.
    ///
    /// BEHAVIOR: When false (default), provides a short view with essential information
    /// and optional parameter counts. When true, includes all CMake/Meson variables
    /// and build configuration details.
    ///
    /// SHORT VIEW (false): Essential paths, build type, generator, parameter count
    /// DETAILED VIEW (true): All build_options variables included
    ///
    /// RECOMMENDED: Use false for general project overview, true only when debugging
    /// build configuration issues or when detailed variable information is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_details: Option<bool>,
}

impl GetProjectDetailsTool {
    #[instrument(name = "get_project_details", skip(self, meta_project))]
    pub fn call_tool(
        &self,
        meta_project: &ProjectWorkspace,
        app_config: &AppConfig,
    ) -> Result<CallToolResult, CallToolError> {
        // Determine if we need to re-scan based on different parameters
        let requested_path = self.path.as_ref().map(std::path::PathBuf::from);
        let requested_depth = self.depth.unwrap_or(meta_project.scan_depth as u32) as usize;

        let needs_rescan = match &requested_path {
            Some(path) => path != &meta_project.project_root_path,
            None => false,
        } || requested_depth != meta_project.scan_depth;

        let fresh_scan = if needs_rescan {
            // Perform fresh scan with user-specified parameters
            let scan_root = requested_path
                .as_ref()
                .unwrap_or(&meta_project.project_root_path);

            info!(
                "Re-scanning project: path={}, depth={} (differs from cached scan)",
                scan_root.display(),
                requested_depth
            );

            Some(self.perform_fresh_scan(scan_root, requested_depth, app_config)?)
        } else {
            // Use cached ProjectWorkspace
            info!("Using cached ProjectWorkspace scan results");
            None
        };

        let effective_meta_project = fresh_scan.as_ref().unwrap_or(meta_project);
        let rescanned_fresh = fresh_scan.is_some();
        let include_details = self.include_details.unwrap_or(false);

        // Create appropriate view based on include_details flag
        let view = if include_details {
            effective_meta_project.get_full_view()
        } else {
            effective_meta_project.get_short_view()
        };

        // Serialize the view
        let mut content = serde_json::to_value(&view).map_err(|e| {
            CallToolError::new(std::io::Error::other(format!(
                "Failed to serialize project view: {e}"
            )))
        })?;

        // Add the rescanned flag which isn't part of the core ProjectWorkspace
        if let Some(obj) = content.as_object_mut() {
            obj.insert(
                "rescanned".to_string(),
                serde_json::Value::Bool(rescanned_fresh),
            );
            // Report the settings the server actually resolved, not the file as
            // written: an agent debugging clangd behaviour needs to see the value
            // that won, and whether a config file was found at all.
            obj.insert("configuration".to_string(), config_view(app_config));
        }

        info!(
            "Successfully analyzed project details: {} components across {} providers: {:?}",
            effective_meta_project.component_count(),
            effective_meta_project.get_provider_types().len(),
            effective_meta_project.get_provider_types()
        );

        Ok(CallToolResult::text_content(vec![TextContent::from(
            serialize_result(&content),
        )]))
    }

    /// Perform a fresh project scan without caching
    fn perform_fresh_scan(
        &self,
        scan_root: &std::path::Path,
        depth: usize,
        app_config: &AppConfig,
    ) -> Result<crate::project::ProjectWorkspace, CallToolError> {
        use crate::project::ProjectScanner;

        // Create project scanner with default providers
        let scanner = ProjectScanner::with_default_providers();

        // Only the depth is overridden by the caller; the remaining scan rules
        // (hidden dirs, symlinks, component cap) stay as the project configured
        // them, so a rescan cannot see a different tree than the startup scan.
        scanner
            .scan_project(scan_root, depth, Some(app_config.scan_options()))
            .map_err(|e| {
                CallToolError::new(std::io::Error::other(format!(
                    "Failed to scan project at {}: {}",
                    scan_root.display(),
                    e
                )))
            })
    }
}

/// Render the resolved configuration for the tool response.
///
/// Deliberately reports effective values rather than echoing the YAML: the
/// whole point is to answer "what is the server actually using", which after
/// CLI and environment overrides is not necessarily what the file says.
fn config_view(config: &AppConfig) -> serde_json::Value {
    let clangd = &config.clangd;
    serde_json::json!({
        "source_file": config.source_file.as_ref().map(|p| p.display().to_string()),
        "clangd": {
            "path": clangd.path,
            "extra_args": clangd.extra_args,
            "background_index": clangd.background_index,
            "pch_storage": format!("{:?}", clangd.pch_storage).to_lowercase(),
            "index_threads": clangd.index_threads,
            "workspace_symbol_limit": clangd.workspace_symbol_limit,
            "initialization_timeout_secs": clangd.initialization_timeout.as_secs(),
            "request_timeout_secs": clangd.request_timeout.as_secs(),
            "index_wait_timeout_secs": clangd.index_wait_timeout.as_secs(),
        },
        "project": {
            "scan_depth": config.project.scan_depth,
            "skip_hidden": config.project.skip_hidden,
            "follow_symlinks": config.project.follow_symlinks,
            "max_components": config.project.max_components,
        },
        "server": {
            "host": config.server.host,
            "port": config.server.port,
        },
    })
}

mod clangd;
mod config;
mod io;
mod logging;
mod lsp;
mod mcp_server;
mod project;
mod symbol;

#[cfg(test)]
mod test_utils;

use clap::Parser;
use config::{AppConfig, CliOverrides, SystemEnv};
use logging::{LogConfig, init_logging};
use mcp_server::CppServerHandler;
use project::{ProjectScanner, ProjectWorkspace};
use rust_mcp_sdk::schema::{
    Implementation, InitializeResult, LATEST_PROTOCOL_VERSION, ServerCapabilities,
    ServerCapabilitiesTools,
};

use rust_mcp_sdk::{
    McpServer, StdioTransport, TransportOptions,
    error::SdkResult,
    mcp_server::{
        HyperServerOptions, McpServerOptions, ToMcpServerHandler, hyper_server, server_runtime,
    },
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Transport to use for the MCP server
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
enum Transport {
    /// Standard input/output transport (spawned by an MCP client)
    Stdio,
    /// Streamable HTTP transport (standalone axum server over TCP)
    Http,
}

/// CLI arguments for the MCP C++ server
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Project root directory to scan for build configurations (defaults to current directory)
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Path to clangd executable (overrides CLANGD_PATH env var and .mcp-cpp.yaml)
    #[arg(long, value_name = "PATH")]
    clangd_path: Option<String>,

    /// Log level (overrides RUST_LOG env var)
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    /// Log file path (overrides MCP_LOG_FILE env var)
    #[arg(long, value_name = "FILE")]
    log_file: Option<PathBuf>,

    /// Transport to serve on: stdio (default) or http (streamable HTTP)
    #[arg(long, value_name = "TRANSPORT", default_value = "stdio")]
    transport: Transport,

    /// Host to bind the streamable-http server to (overrides .mcp-cpp.yaml; default 127.0.0.1)
    #[arg(long, value_name = "HOST")]
    host: Option<String>,

    /// Port to bind the streamable-http server to (overrides .mcp-cpp.yaml; default 8080)
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,
}

/// Create ProjectWorkspace with all project setup logic centralized
fn create_project_workspace(config: &AppConfig) -> ProjectWorkspace {
    let project_root = config.project_root.clone();
    let depth = config.project.scan_depth;

    info!(
        "Scanning project root for build configurations: {} (depth: {})",
        project_root.display(),
        depth
    );

    // Create project scanner with default providers
    let scanner = ProjectScanner::with_default_providers();

    match scanner.scan_project(&project_root, depth, Some(config.scan_options())) {
        Ok(project_workspace) => {
            info!(
                "Successfully discovered {} components across {} providers: {:?}",
                project_workspace.component_count(),
                project_workspace.get_provider_types().len(),
                project_workspace.get_provider_types()
            );
            project_workspace
        }
        Err(e) => {
            eprintln!(
                "Failed to scan project at {}: {}",
                project_root.display(),
                e
            );
            // Create empty ProjectWorkspace as fallback
            ProjectWorkspace::new(project_root, Vec::new(), depth)
        }
    }
}

#[tokio::main]
async fn main() -> SdkResult<()> {
    let args = Args::parse();

    // Extract values before moving
    let log_level = args.log_level.clone();
    let log_file = args.log_file.clone();
    let root_arg = args.root.clone();

    // Initialize logging with configuration from env vars and CLI args
    let log_config = LogConfig::from_env().with_overrides(log_level, log_file);

    if let Err(e) = init_logging(log_config) {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    // Resolve project root directory
    let project_root = root_arg.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|e| {
            eprintln!("Failed to get current directory: {e}");
            std::process::exit(1);
        })
    });

    // Resolve layered configuration: CLI > env > .mcp-cpp.yaml > defaults.
    let cli_overrides = CliOverrides {
        clangd_path: args.clangd_path.clone(),
        host: args.host.clone(),
        port: args.port,
    };
    let app_config = match AppConfig::resolve(&project_root, &cli_overrides, &SystemEnv) {
        Ok(config) => Arc::new(config),
        Err(e) => {
            // A config file that exists but cannot be understood is fatal. Starting
            // anyway would silently ignore settings the user deliberately wrote.
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    match &app_config.source_file {
        Some(path) => info!("Loaded configuration from {}", path.display()),
        None => info!("No .mcp-cpp.yaml found; using defaults"),
    }

    // Create ProjectWorkspace with all project setup
    let project_workspace = create_project_workspace(&app_config);

    info!(
        "Starting C++ MCP Server with project root: {}",
        project_workspace.project_root_path.display()
    );

    // Define server details and capabilities
    let server_details = InitializeResult {
        server_info: Implementation {
            name: "C++ MCP Server".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            title: Some("C++ Project Analysis MCP Server".to_string()),
            description: None,
            icons: vec![],
            website_url: None,
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        meta: None,
        instructions: Some("C++ project analysis and LSP bridge server".to_string()),
        protocol_version: LATEST_PROTOCOL_VERSION.to_string(),
    };

    info!("Using clangd: {}", app_config.clangd.path);

    // Create custom handler with ProjectWorkspace and resolved configuration
    let handler = match CppServerHandler::new(project_workspace, Arc::clone(&app_config)) {
        Ok(handler) => handler,
        Err(e) => {
            eprintln!("Failed to create server handler: {}", e);
            std::process::exit(1);
        }
    };

    match args.transport {
        Transport::Stdio => {
            // Create stdio transport
            let transport = StdioTransport::new(TransportOptions::default())?;

            // Create MCP server
            let server = server_runtime::create_server(McpServerOptions {
                server_details,
                transport,
                handler: handler.to_mcp_server_handler(),
                task_store: None,
                client_task_store: None,
            });

            info!("C++ MCP Server ready and listening for requests (stdio)");

            // Start the server
            if let Err(start_error) = server.start().await {
                eprintln!(
                    "{}",
                    start_error
                        .rpc_error_message()
                        .unwrap_or(&start_error.to_string())
                );
            }
        }
        Transport::Http => {
            let http_options = HyperServerOptions {
                host: app_config.server.host.clone(),
                port: app_config.server.port,
                transport_options: Arc::new(TransportOptions::default()),
                // Return plain JSON instead of SSE streams for simpler clients.
                enable_json_response: Some(true),
                // The SDK's single-shot JSON path reads only the first SSE line as
                // the response. With the default 12s ping interval, a request that
                // takes longer than 12s (e.g. an LSP query on a large codebase)
                // would have its first line be a keepalive ping, yielding an empty
                // body. Raise the interval so the actual response is always first.
                ping_interval: Duration::from_secs(3600),
                ..Default::default()
            };
            let base_url = http_options.base_url();
            let endpoint = http_options.streamable_http_url();

            let http_server = hyper_server::create_server(
                server_details,
                handler.to_mcp_server_handler(),
                http_options,
            );

            info!("C++ MCP Server ready at {} (streamable-http)", endpoint);
            eprintln!("MCP streamable HTTP server listening at {}", endpoint);
            eprintln!("  base URL: {}", base_url);

            // Start the server
            if let Err(start_error) = http_server.start().await {
                eprintln!(
                    "{}",
                    start_error
                        .rpc_error_message()
                        .unwrap_or(&start_error.to_string())
                );
            }
        }
    }

    Ok(())
}

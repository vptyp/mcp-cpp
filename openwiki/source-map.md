---
type: Reference
title: Source Map
description: Directory-by-directory map of the src/ tree and key supporting directories with file roles and notable types.
resource: https://github.com/mpsm/mcp-cpp
tags: [source-map, repository, rust]
openwiki:
  roles: [repository]
  invariants:
    - src/ is organized as five layers - mcp_server, project, clangd, lsp, io - plus symbol and logging utilities
---

# Source Map

## `src/main.rs`

Entrypoint. Parses clap CLI args (`--root`, `--clangd-path`, `--log-level`, `--log-file`, `--transport`, `--host`, `--port`), initializes tracing, scans the project root at depth 3, creates `CppServerHandler`, and starts the rust-mcp-sdk stdio or streamable-HTTP server.

## `src/mcp_server/`

| File | Role |
|---|---|
| `mod.rs` | Re-exports `CppServerHandler` |
| `server.rs` | `CppServerHandler` - implements `ServerHandler`, `McpToolHandler` impls per tool, `register_tools!` dispatch, `handle_list_tools_request`, `handle_call_tool_request` |
| `server_helpers.rs` | `resolve_build_directory` (auto-detect or validate; dynamic discovery) |
| `tools/mod.rs` | Tool module registry |
| `tools/project_tools.rs` | `GetProjectDetailsTool` |
| `tools/search_symbols.rs` | `SearchSymbolsTool` + `SearchResult`/`SearchMetadata` |
| `tools/analyze_symbols.rs` | `AnalyzeSymbolContextTool` + `AnalyzerResult`/`AnalyzerError`; aggregates LSP helpers |
| `tools/show_diagnostics.rs` | `ShowDiagnosticsTool` + `DiagnosticsResult` |
| `tools/index_status.rs` | `GetIndexStatusTool` + `IndexStatusResult` |
| `tools/utils.rs` | `serialize_result`, `handle_selective_indexing_wait` |
| `tools/lsp_helpers/` | `document_symbols.rs` (symbol resolution/search builder, largest), `workspace_symbols.rs`, `call_hierarchy.rs`, `type_hierarchy.rs`, `definitions.rs`, `hover.rs`, `members.rs`, `examples.rs`, `symbol_resolution.rs` |
| `tools/tests/` | Feature-gated tool tests |

## `src/project/`

| File | Role |
|---|---|
| `mod.rs` | Re-exports: `CmakeProvider`, `MesonProvider`, `ProjectComponent`, `ComponentSession`, `ProjectError`, `ProjectScanner`, `ProjectWorkspace`, `WorkspaceSession`, `CompilationDatabase`, `BuildOptions` |
| `workspace.rs` | `ProjectWorkspace`, `ProjectWorkspaceView`, `ProjectComponentView` (short/full views) |
| `workspace_session.rs` | `WorkspaceSession` + `CreationSlot` (session cache + in-flight dedup) |
| `component.rs` | `ProjectComponent` (validated build config) |
| `component_session.rs` | `ComponentSession` (owns clangd, file manager, diagnostics, index monitor) |
| `scanner.rs` | `ProjectScanner` + `ScanOptions` |
| `provider.rs` | `ProjectComponentProvider` trait + `ProjectProviderRegistry` |
| `cmake_provider.rs` | `CmakeProvider` (parses `CMakeCache.txt`) |
| `meson_provider.rs` | `MesonProvider` (parses `meson-info/intro-*.json`) |
| `compilation_database.rs` | `CompilationDatabase` + `BuildOptions` aggregation |
| `error.rs` | `ProjectError` |
| `index/component_monitor.rs` | `ComponentIndexMonitor` + `ComponentIndexingState` |
| `index/reader.rs` | `IndexReader`/`IndexReaderTrait` |
| `index/state.rs` | `IndexState` for CDB entries |
| `index/status.rs` | `IndexStatusView` |
| `index/trigger.rs` | `IndexTrigger` trait + `ClangdIndexTrigger` |
| `index/storage/` | `IndexStorage` trait + `FilesystemIndexStorage` |
| `index/integration_tests.rs` | Feature-gated project index tests |

## `src/clangd/`

| File | Role |
|---|---|
| `mod.rs` | Re-exports session + index types |
| `config.rs` | `ClangdConfig`, `ClangdConfigBuilder`, `LspConfig`, `ResourceConfig`, `PchStorage`, `ProcessPriority` |
| `session.rs` | `ClangdSession<P,C>`, `ClangdSessionTrait`, `close()` + Drop safety net |
| `session_builder.rs` | `ClangdSessionBuilder` (phantom-typed, production vs test build) |
| `version.rs` | `ClangdVersion::detect` + `index_format_version()` mapping |
| `log_monitor.rs` | `LogMonitor` + `ClangdLogParser` (stderr -> ProgressEvent) |
| `file_manager.rs` | `ClangdFileManager` (LSP document sync, SHA-256 change detection) |
| `diagnostics.rs` | `DiagnosticsCollector` (captures publishDiagnostics) |
| `error.rs` | `ClangdSessionError`, `ClangdConfigError` |
| `testing.rs` | `MockClangdSession`, test config/session helpers |
| `index/component_index.rs` | `ComponentIndex` (pure data: source -> .idx mapping + state) |
| `index/idx_parser.rs` | `IdxParser` (RIFF/CdIx binary parser, formats 12-20) |
| `index/hash.rs` | `compute_file_hash` (xxHash64 <=18, xxh3 >=19) |
| `index/progress_monitor.rs` | `IndexProgressMonitor` (LSP `$/progress` for `backgroundIndexProgress`) |
| `index/latch.rs` | `IndexLatch` (watch-channel completion latch) |
| `index/progress_events.rs` | `ProgressEvent` enum |

## `src/lsp/`

| File | Role |
|---|---|
| `mod.rs` | Re-exports `LspClient`, `LspError` |
| `framing.rs` | `LspFraming<T>` (Content-Length framing, 16 MB cap) |
| `protocol.rs` | `JsonRpcClient<T>`, `JsonRpcRequest/Response/Notification`, `JsonRpcMessage`, `JsonRpcError` |
| `client.rs` | `LspClient<T>` (typed LSP methods over `lsp_types`), `LspError` |
| `traits.rs` | `LspClientTrait` (mockall-automocked) |
| `jsonrpc_utils.rs` | JSON-RPC constants and response builders |
| `testing.rs` | Re-exports `MockLspClientTrait`, `MockTransport`, `MockProcessManager` |

## `src/io/`

| File | Role |
|---|---|
| `mod.rs` | Re-exports `FileBuffer`, `ChildProcessManager`, `StdioTransport`, etc. |
| `process.rs` | `ProcessManager` trait, `ChildProcessManager`, `StopMode`, `ProcessState`, `StderrMonitor`, `ProcessError` |
| `transport.rs` | `Transport` trait, `StdioTransport` (UTF-8-safe channel-based), `MockTransport` |
| `file_buffer.rs` | `FileBuffer<F>` (UTF-8-normalized, line-indexed, auto-refresh), `FilePosition`, `FileBufferError` |
| `file_manager.rs` | `FileBufferManager<F>` (path -> buffer cache), `RealFileBufferManager` |
| `file_system.rs` | `FileSystemTrait`, `RealFileSystem`, `TestFileSystem`, `FileMetadata` |

## `src/symbol/`

| File | Role |
|---|---|
| `mod.rs` | Re-exports |
| `symbol.rs` | `Symbol` (name, kind, container_name, location) + conversions from `WorkspaceSymbol`/`DocumentSymbol` |
| `location.rs` | `FileLocation`, `Position`, `Range` |

## `src/logging.rs`

`LogConfig` + `init_logging` with env-var and CLI overrides (`RUST_LOG`, `MCP_LOG_FILE`, `MCP_LOG_UNIQUE`).

## Supporting directories

| Directory | Contents |
|---|---|
| `tools/` | Python: `lsp-cli.py`, `generate-index.py`, `clangd-idx-viewer.py`, `read-cmake-cache.py`, `requirements.txt` |
| `test/test-project/` | C++20 CMake fixture (10 headers, 7 sources) |
| `test/test-meson-project/` | C++17 Meson fixture |
| `test/requests/` | 11 raw JSON-RPC replay payloads |
| `test/e2e/` | TypeScript/Vitest E2E framework and suites |
| `docs/` | `clangd_index_spec.md`, `symbol_context_analyzer_implementation.md`, `symbol_search_explorer_implementation.md` |
| `docker/` | `entrypoint.sh` |
| `.github/workflows/` | `ci.yml`, `release.yml`, `openwiki-update.yml` |
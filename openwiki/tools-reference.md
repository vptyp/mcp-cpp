---
type: Reference
title: MCP Tools Reference
description: Complete schema, behavior, and output description for the five MCP tools - get_project_details, search_symbols, analyze_symbol_context, show_diagnostics, get_index_status.
resource: https://github.com/mpsm/mcp-cpp
tags: [mcp, tools, api, reference]
openwiki:
  roles: [public-api, integration]
  source_paths:
    - src/mcp_server/tools/project_tools.rs
    - src/mcp_server/tools/search_symbols.rs
    - src/mcp_server/tools/analyze_symbols.rs
    - src/mcp_server/tools/show_diagnostics.rs
    - src/mcp_server/tools/index_status.rs
    - src/mcp_server/server.rs
  symbols:
    - GetProjectDetailsTool
    - SearchSymbolsTool
    - AnalyzeSymbolContextTool
    - ShowDiagnosticsTool
    - GetIndexStatusTool
    - CppServerHandler
    - register_tools!
  invariants:
    - Tools are registered via the register_tools! macro and dispatched by name in CppServerHandler
    - build_directory auto-detects when exactly one build dir exists and errors when zero or many exist without an explicit choice
    - Every tool that touches clangd resolves a ComponentSession first, which lazily spawns clangd
    - search_symbols and analyze_symbol_context wait for indexing up to wait_timeout (default 20s) then attach index_status
  validation_commands:
    - cargo test --lib mcp_server
---

# MCP Tools Reference

The server exposes five tools, registered in `src/mcp_server/server.rs` via the `register_tools!` macro. Each tool struct is annotated with `#[mcp_tool(name = "...", description = "...")]` and derives `JsonSchema`, so its input schema is published to MCP clients through `tools/list`. All tool output is JSON returned as `TextContent`.

## Tool dispatch

`CppServerHandler::handle_call_tool_request` logs the incoming call, calls `self.dispatch_tool(&tool_name, params.arguments)` (macro-generated), and logs timing. Each tool handler resolves the build directory, obtains a `ComponentSession` from `WorkspaceSession`, snapshots the `ProjectComponent`, and calls the tool's async `call_tool`.

## Common parameters

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `build_directory` | `Option<String>` | auto-detect | Absolute path strongly preferred. Auto-detection succeeds only when exactly one build dir exists. |
| `wait_timeout` | `Option<u64>` | `20` (seconds) | Time to wait for clangd indexing/diagnostics. `0` = no wait. |

---

## `get_project_details`

Source: `src/mcp_server/tools/project_tools.rs` - `GetProjectDetailsTool`.

**Purpose**: Discover build configurations and return absolute build directory paths for use with the other tools. This is the recommended first call for any agent.

**Parameters**:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `path` | `Option<String>` | cached scan root | Project root to scan. A value different from the cached scan triggers a fresh (uncached) scan. Avoid `"."`. |
| `depth` | `Option<u32>` | cached scan depth (3) | Scan depth 0-10. |
| `include_details` | `Option<bool>` | `false` | When false, returns a short view with `build_options_count` instead of full options to avoid context exhaustion. |

**Behavior**: If `path`/`depth` differ from the cached scan, performs a fresh `ProjectScanner` scan. Otherwise returns the cached `ProjectWorkspace`. Serializes a `ProjectWorkspaceView` (short or full) and adds a `rescanned: bool` field.

**Output**: A `ProjectWorkspaceView` JSON object containing `project_root_path`, `components` (each with `build_dir_path`, `source_root_path`, `compilation_database_path`, `provider_type`, `generator`, `build_type`, `build_options` or `build_options_count`, aggregated `compiler_options`), `scan_depth`, `discovered_at`, optional `global_compilation_database_path`, and `rescanned`.

---

## `search_symbols`

Source: `src/mcp_server/tools/search_symbols.rs` - `SearchSymbolsTool`.

**Purpose**: Find C++ symbols by name across the workspace or enumerate all symbols in specific files.

**Parameters**:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `query` | `String` | required | C++ **symbol name** (not a file path). `""` = exploration mode. |
| `kinds` | `Option<Vec<String>>` | none | PascalCase symbol kinds: `Class`, `Function`, `Method`, `Variable`, `Enum`, `Namespace`, `Constructor`, `Field`, `Interface`, `Struct`. |
| `files` | `Option<Vec<String>>` | none | File paths for document-specific search. Triggers document-symbol mode. |
| `max_results` | `Option<u32>` | `100` (max 1000) | Client-side result limit. |
| `include_external` | `Option<bool>` | `false` | Include system/library symbols. |
| `build_directory` | `Option<String>` | auto-detect | Absolute path preferred. |
| `wait_timeout` | `Option<u64>` | `20` | Indexing wait timeout; `0` = no wait. |

**Dual search modes**:

- **Workspace search** (when `files` is `None`): uses LSP `workspace/symbol`. Subject to clangd's internal heuristics and relevance ranking; may not return every match. Sends a fixed 2000-symbol query to clangd and applies `max_results` client-side for consistent ranking.
- **Document search** (when `files` is provided): uses `textDocument/documentSymbol` per file. Returns all symbols in each file matching the query (substring match); more predictable and complete.

**Output**: A `SearchResult` JSON object with `success`, `query`, `total_matches`, `symbols` (each a `Symbol`: `name`, `kind`, `container_name`, `location`), `metadata` (`search_type`, `build_directory`, per-file `files_processed`), and optional `index_status` (an `IndexStatus` from `src/clangd/progress.rs`, attached when clangd's indexing is not yet complete after the wait or the wait was skipped).

---

## `analyze_symbol_context`

Source: `src/mcp_server/tools/analyze_symbols.rs` - `AnalyzeSymbolContextTool`.

**Purpose**: Deep, multi-dimensional analysis of one C++ symbol. Aggregates many LSP calls (definition, declaration, hover, references, document symbols, call hierarchy, type hierarchy) into a single result.

**Parameters**:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `symbol` | `String` | required | C++ symbol name. Supports simple (`MyClass`), qualified (`MyNamespace::MyClass`), global scope (`::main`), and method (`MyClass::method`) forms. Not a file path. |
| `build_directory` | `Option<String>` | auto-detect | Absolute path preferred. |
| `max_examples` | `Option<u32>` | unlimited | Limit on usage example snippets. |
| `location_hint` | `Option<String>` | none | Disambiguation hint for overloads/templates, format `"/abs/path/file.cpp:line:column"` (1-based). |
| `wait_timeout` | `Option<u64>` | `20` | Indexing wait timeout; `0` = no wait. |

**Symbol resolution**: Without a `location_hint`, uses workspace symbol resolution (fuzzy matching). With a hint, finds the document symbol at the specified position.

**Output**: An `AnalyzerResult` JSON object with `symbol` (a `Symbol`), `query`, `definitions` (Vec of `FileLocation`), optional `declarations`, optional `hover_documentation` and `detail`, `examples` (usage snippets as `FileLocation`s), optional `type_hierarchy` (base and derived classes), optional `members` (class fields/methods/constructors), and optional `call_hierarchy` (incoming/outgoing calls). Inheritance, call relationships, and usage patterns are included automatically when applicable to the symbol kind. LSP helpers live in `src/mcp_server/tools/lsp_helpers/`.

---

## `show_diagnostics`

Source: `src/mcp_server/tools/show_diagnostics.rs` - `ShowDiagnosticsTool`.

**Purpose**: Retrieve clangd's semantic diagnostics (compile errors, warnings, notes) for a single source file.

**Parameters**:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `file` | `String` | required | Path to a C++ source file. Relative paths are resolved against the component's `source_root_path`; absolute paths used as-is. |
| `build_directory` | `Option<String>` | auto-detect | Absolute path preferred. |
| `wait_timeout` | `Option<u64>` | `20` | Time to wait for clangd to publish diagnostics; `0` = no wait. |

**Behavior**: Diagnostics are *pushed* by clangd only after a file is opened. The tool resets the `DiagnosticsCollector` for the file URI, calls `ClangdFileManager::ensure_file_ready` (which triggers `didOpen` and a fresh parse), then `DiagnosticsCollector::wait_for_uri` polls every 50ms until diagnostics arrive or the timeout expires. This is a document-specific operation, so it skips the workspace indexing wait and returns current index status instead.

**Output**: A `DiagnosticsResult` JSON object with `success`, `file`, `build_directory`, `total`, `errors`, `warnings`, `notes` (severity counts), `timed_out` (bool), `diagnostics` (Vec of `lsp_types::Diagnostic` with severity, message, range), and optional `index_status`. Empty diagnostic lists are stored deliberately so a clean file is distinguishable from "clangd hasn't published yet".

---

## `get_index_status`

Source: `src/mcp_server/tools/index_status.rs` - `GetIndexStatusTool`.

**Purpose**: Lightweight indexing progress report for a build directory, directly from clangd's own background-index progress notifications - no cache files or clangd logs are read. Useful for monitoring a long background index build (e.g. a large Chromium tree).

**Parameters**:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `build_directory` | `Option<String>` | auto-detect | Absolute path preferred. |
| `wait_timeout` | `Option<u64>` | `0` | Seconds to wait for clangd's current indexing pass to finish before returning; `0` = return immediately. |

**Output**: An `IndexStatusResult` JSON object with `success`, `build_directory`, and `index_status` (an `IndexStatus` from `src/clangd/progress.rs`: `state` - one of `NotStarted`/`InProgress`/`Completed`; `in_progress` bool; optional `percentage` (0-100); optional `message`). The status mirrors clangd's `$/progress` notifications on the `backgroundIndexProgress` token; `state` becomes `Completed` with `percentage: 100` on the `end` progress kind.
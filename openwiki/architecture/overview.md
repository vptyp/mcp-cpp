---
type: Reference
title: Architecture
description: Layered architecture of mcp-cpp-server from MCP handler down through project/workspace sessions, clangd sessions, the LSP client, and the IO layer.
resource: https://github.com/mpsm/mcp-cpp
tags: [architecture, mcp, clangd, lsp, rust]
openwiki:
  roles: [architecture]
  source_paths: [src/main.rs, src/mcp_server/server.rs, src/project/workspace_session.rs, src/project/component_session.rs, src/clangd/session.rs, src/clangd/session_builder.rs, src/lsp/client.rs, src/lsp/protocol.rs, src/io/transport.rs, src/io/process.rs]
  invariants:
    - One clangd process per build directory, created lazily and cached per WorkspaceSession
    - compile_commands.json is mandatory for any ClangdConfig
    - In-flight ComponentSession creation is deduplicated so concurrent requests share one clangd
    - Stderr is always drained to prevent clangd blocking on a full pipe
    - Drop of an unclosed ClangdSession synchronously SIGKILLs the process as a safety net
---

# Architecture

The server is a layered Rust application. Each layer is a module under `src/`, and dependencies flow strictly downward: the MCP handler calls the project/workspace layer, which calls the clangd session layer, which calls the LSP client, which calls the IO transport, which talks to the clangd process.

```mermaid
flowchart TD
    subgraph MCP["src/mcp_server"]
        Handler["CppServerHandler + register_tools!"]
        Tools["Tools: search, analyze, diagnostics, project, index_status"]
    end
    subgraph Proj["src/project"]
        WS["ProjectWorkspace / WorkspaceSession"]
        CS["ComponentSession"]
        Scanner["ProjectScanner + Providers (CMake, Meson)"]
        Idx["ComponentIndexMonitor"]
    end
    subgraph CD["src/clangd"]
        Sess["ClangdSession + Builder"]
        Fm["ClangdFileManager"]
        Diag["DiagnosticsCollector"]
        Log["LogMonitor"]
        CIdx["ComponentIndex / IdxParser"]
    end
    subgraph LSP["src/lsp"]
        Client["LspClient"]
        Rpc["JsonRpcClient"]
        Frame["LspFraming"]
    end
    subgraph IO["src/io"]
        Stdio["StdioTransport"]
        Proc["ChildProcessManager"]
        FB["FileBuffer / FileSystem"]
    end
    ClangdProc["clangd process"]

    Handler --> Tools
    Tools --> WS
    Tools --> CS
    WS --> Scanner
    WS --> CS
    CS --> Sess
    CS --> Fm
    CS --> Diag
    CS --> Idx
    Idx --> CIdx
    Sess --> Client
    Sess --> Log
    Client --> Rpc
    Rpc --> Frame
    Frame --> Stdio
    Stdio --> Proc
    Proc --> ClangdProc
    Tools --> FB
```

## Startup path

`main.rs` is the entrypoint. It parses CLI args with clap, initializes `tracing` logging, resolves the clangd path (CLI arg > `CLANGD_PATH` env > `"clangd"`), and scans the project root with `ProjectScanner::with_default_providers()` at depth 3. The resulting `ProjectWorkspace` and clangd path are handed to `CppServerHandler::new`, which wraps them in a `WorkspaceSession`. Depending on `--transport`, it then starts the rust-mcp-sdk stdio server or an HTTP (axum) server.

```mermaid
sequenceDiagram
    participant Main as main()
    participant Scanner as ProjectScanner
    participant Handler as CppServerHandler
    participant SDK as rust-mcp-sdk
    Main->>Scanner: scan_project(root, depth=3)
    Scanner-->>Main: ProjectWorkspace
    Main->>Handler: new(workspace, clangd_path)
    Handler->>Handler: WorkspaceSession::new (detect clangd version)
    Main->>SDK: start stdio or http server
    SDK->>Handler: handle_call_tool_request on each request
```

## The five layers

### 1. MCP server layer (`src/mcp_server`)

`CppServerHandler` (`src/mcp_server/server.rs`) implements the rust-mcp-sdk `ServerHandler` trait. It holds a single `WorkspaceSession`. The `register_tools!` macro generates a compile-time-safe dispatch table mapping tool names to async handlers. Each tool struct is annotated with `#[mcp_tool(...)]`, which derives its JSON schema for `tools/list`. See [MCP Tools](tools-reference.md).

### 2. Project / workspace layer (`src/project`)

This layer turns a filesystem tree into a set of build configurations and manages per-component sessions:

- `ProjectScanner` + `ProjectProviderRegistry` walk the tree; each `ProjectComponentProvider` (CMake, Meson) detects build directories and produces a `ProjectComponent`.
- `ProjectWorkspace` is the immutable result of a scan - root path, components, scan depth, discovery timestamp, optional global compilation database.
- `WorkspaceSession` owns the workspace behind an `Arc<Mutex>` and manages a map of build directory to `Arc<ComponentSession>`. It deduplicates in-flight session creation with a `CreationSlot`/`Notify` pattern so concurrent tool calls for the same build dir share one clangd instead of racing to spawn two.
- `ComponentSession` owns one clangd: a `ClangdSession` (behind `Arc<Mutex>`), a `ClangdFileManager`, a `DiagnosticsCollector`, and a `ComponentIndexMonitor`.

See [Project and Workspace](project-workspace.md).

### 3. clangd session layer (`src/clangd`)

`ClangdSession<P, C>` is generic over a `ProcessManager` and an `LspClientTrait` so tests can inject mocks. The production concrete type is `ClangdSession<ChildProcessManager, LspClient<StdioTransport>>`. A phantom-typed `ClangdSessionBuilder` enforces at compile time that you cannot `build()` without a config, and selects the real spawn-and-initialize path versus the test path based on whether dependencies were injected.

The session owns:
- a `LogMonitor` that parses clangd stderr into `ProgressEvent`s,
- an `IndexProgressMonitor` that listens for LSP `$/progress` notifications on the `backgroundIndexProgress` token,
- the `LspClient` for typed requests.

`ClangdFileManager` tracks open documents and keeps clangd in sync with `didOpen`/`didChange`, using SHA-256 content hashing to avoid redundant notifications. `DiagnosticsCollector` captures `publishDiagnostics` notifications.

See [Clangd Session and Indexing](clangd-session.md).

### 4. LSP layer (`src/lsp`)

A clean three-layer stack over the generic `Transport` trait:

- `LspFraming<T>` adds the `Content-Length: N\r\n\r\n<content>` framing (capped at 16 MB).
- `JsonRpcClient<T>` is the async JSON-RPC 2.0 engine: it spawns a transport task, correlates responses by `u64` id, fans notifications out to all handlers, and dispatches requests to a single request handler.
- `LspClient<T>` wraps `JsonRpcClient` with typed `lsp_types` request/notification methods so method strings are compile-time-checked. It advertises capabilities (hover markdown, definition with `linkSupport`, document symbol with all 26 symbol kinds, `workDoneProgress`) and handles the clangd quirk where `linkSupport` is ignored and `Location[]` is returned instead of `LocationLink[]`.

`LspClientTrait` (with `mockall::automock`) abstracts the client for session polymorphism.

### 5. IO layer (`src/io`)

Protocol-agnostic primitives:

- `ChildProcessManager` spawns clangd with piped stdio, takes all three streams immediately, always drains stderr (even with no handler, to avoid pipe-full blocking), and tracks process state. `stop()` sends SIGTERM then SIGKILL on Unix; `kill_sync()` is the Drop safety net. Windows emits warnings instead.
- `StdioTransport` is channel-based: a stdin writer task and a stdout reader task that accumulates raw bytes and emits only complete valid UTF-8 prefixes, retaining partial multibyte sequences across reads.
- `FileBuffer<F>` is a UTF-8-normalized, line-indexed file view with auto-refresh on modification; columns are UTF-8 code points, not bytes. `FileBufferManager` caches buffers by path with a manager-owned filesystem for DI.

## Request flow: a tool call

```mermaid
sequenceDiagram
    participant Client as MCP Client
    participant Handler as CppServerHandler
    participant WS as WorkspaceSession
    participant CS as ComponentSession
    participant Clangd as clangd
    Client->>Handler: tools/call search_symbols { build_directory }
    Handler->>Handler: resolve_build_directory
    Handler->>WS: get_component_session(build_dir)
    WS->>CS: get or create (dedup concurrent)
    CS->>Clangd: LSP workspace/symbol
    Clangd-->>CS: symbols
    CS->>CS: filter project vs external, kind filter, limit
    CS-->>Handler: SearchResult JSON
    Handler-->>Client: CallToolResult
```

The handler resolves the build directory (auto-detect if one, require explicit choice if many, or dynamically discover a hint path), gets or creates a `ComponentSession`, snapshots the component under a short lock, and runs the tool. The workspace lock is never held across a slow clangd query.

## Concurrency and locking discipline

- `WorkspaceSession` holds `Arc<Mutex<ProjectWorkspace>>`. Build-directory resolution takes the lock briefly, then releases it before session creation.
- `ComponentSession` wraps its `ClangdSession` and `ClangdFileManager` in `Arc<Mutex<...>>` so background tasks (progress-event processing, indexing triggers) can share them.
- In-flight session creation uses a `CreationSlot` with `tokio::sync::Notify` - the first request to arrive registers as the creator; later concurrent requests await the same slot and receive the shared result.
- LSP notification handlers (`IndexProgressMonitor`, `DiagnosticsCollector`) spawn a background `tokio::task` per notification so the transport loop is never blocked.
- Progress events are sent with `try_send` - progress is best-effort and never back-pressures clangd.
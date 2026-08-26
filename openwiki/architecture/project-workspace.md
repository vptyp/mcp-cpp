---
type: Reference
title: Project and Workspace Subsystem
description: Project scanning, multi-provider build discovery (CMake/Meson), and the WorkspaceSession/ComponentSession lifecycle that manages per-build-directory clangd processes.
resource: https://github.com/mpsm/mcp-cpp
tags: [project, workspace, cmake, meson, build-system, rust]
openwiki:
  roles: [architecture, domain]
  source_paths:
    - src/project/mod.rs
    - src/project/scanner.rs
    - src/project/provider.rs
    - src/project/cmake_provider.rs
    - src/project/meson_provider.rs
    - src/project/workspace.rs
    - src/project/workspace_session.rs
    - src/project/component.rs
    - src/project/component_session.rs
    - src/project/compilation_database.rs
    - src/project/error.rs
  symbols:
    - ProjectScanner
    - ProjectProviderRegistry
    - ProjectComponentProvider
    - CmakeProvider
    - MesonProvider
    - ProjectWorkspace
    - ProjectWorkspaceView
    - ProjectComponent
    - ProjectComponentView
    - WorkspaceSession
    - ComponentSession
    - CompilationDatabase
    - BuildOptions
    - ProjectError
  invariants:
    - ProjectScanner walks the tree with walkdir bounded by scan depth, skipping hidden dirs by default
    - Each provider returns Ok(None) for dirs it cannot handle; the registry returns the first match
    - A ProjectComponent requires existing build dir, source root, and compile_commands.json
    - WorkspaceSession deduplicates concurrent ComponentSession creation so only one clangd spawns per build dir
    - Dynamic component discovery creates a component from a hint build directory not in the initial scan
---

# Project and Workspace Subsystem

The `src/project` module turns a filesystem tree into a set of build configurations and manages the per-component sessions that own clangd processes. It is built around an extensible **provider pattern** so new build systems can be added without changing the scanner.

## Scanning and provider discovery

`ProjectScanner` (`src/project/scanner.rs`) holds a `ProjectProviderRegistry` and walks the tree with `walkdir`, bounded by a `depth` parameter and `ScanOptions` (skip hidden dirs by default, no symlink following, optional `max_components`). For each directory, `ProjectProviderRegistry::scan_directory` tries each registered `ProjectComponentProvider` in order; the first that returns `Ok(Some(component))` wins, and a provider returns `Ok(None)` for directories it cannot handle.

`ProjectScanner::with_default_providers()` registers `CmakeProvider` and `MesonProvider`. `discover_component(build_dir)` scans a single directory (no traversal) for dynamic discovery when a tool references a build directory that was not in the initial scan.

### CMake provider

`CmakeProvider` (`src/project/cmake_provider.rs`) detects a CMake build directory by locating `CMakeCache.txt` and parsing it line by line. It extracts:
- `CMAKE_GENERATOR` -> `generator` (e.g. `Ninja`, `Unix Makefiles`)
- `CMAKE_BUILD_TYPE` -> `build_type` (e.g. `Debug`, `Release`)
- `CMAKE_SOURCE_DIR` -> `source_root_path` (falls back to `<PROJECT_NAME>_SOURCE_DIR`)
- all other keys -> `build_options` map

It locates `compile_commands.json` in the build directory.

### Meson provider

`MesonProvider` (`src/project/meson_provider.rs`) detects a Meson build directory by locating a `meson-info` directory. It parses:
- `meson-info/intro-buildoptions.json` -> `build_options`
- `meson-info/intro-projectinfo.json` -> `source_dir`
- `meson-info/intro-buildsystem_files.json` (fallback source dir)
- `meson-info/intro-targets.json` (for target metadata)

The provider type string is `"cmake"` or `"meson"`; the generator for Meson is typically `"ninja"`.

## Data model

```mermaid
erDiagram
    ProjectWorkspace ||--o{ ProjectComponent : contains
    ProjectComponent ||--|| CompilationDatabase : references
    WorkspaceSession ||--|| ProjectWorkspace : owns
    WorkspaceSession ||--o{ ComponentSession : manages
    ComponentSession ||--|| ProjectComponent : represents
    ComponentSession ||--|| ClangdSession : owns
    ComponentSession ||--|| ClangdFileManager : owns
    ComponentSession ||--|| DiagnosticsCollector : owns
    ComponentSession ||--|| ComponentIndexMonitor : owns
    ProjectWorkspace ||--o{ ProjectComponentView : "get_short_view / get_full_view"
    ProjectWorkspaceView ||--o{ ProjectComponentView : contains

    ProjectComponent {
        PathBuf build_dir_path
        PathBuf source_root_path
        PathBuf compilation_database_path
        String provider_type
        String generator
        String build_type
        HashMap build_options
    }
    ProjectWorkspace {
        PathBuf project_root_path
        Vec components
        usize scan_depth
        DateTime discovered_at
        Option global_compilation_database
    }
```

`ProjectComponent::new` validates that the build directory and source root exist and are directories, and that `compilation_database_path` exists.

`ProjectWorkspace` provides `get_short_view()` (excludes full `build_options`, includes `build_options_count` and aggregated `compiler_options` from a bounded sample of `compile_commands.json`) and `get_full_view()` (includes all `build_options`). The short view exists to prevent context-window exhaustion when agents call `get_project_details`.

`CompilationDatabase` (`src/project/compilation_database.rs`) wraps `json_compilation_db` and provides `BuildOptions` aggregation (defines, include paths, language standard, optimization, flags) from a bounded sample of entries.

## Session lifecycle

### WorkspaceSession

`WorkspaceSession` (`src/project/workspace_session.rs`) is the long-lived owner created in `main.rs`. It:
- detects the clangd version once at construction (`ClangdVersion::detect`) to select the correct index format hash,
- holds `Arc<Mutex<ProjectWorkspace>>` so dynamic discovery can add components,
- maintains `component_sessions: Arc<Mutex<HashMap<PathBuf, Arc<ComponentSession>>>>`,
- deduplicates in-flight creation with `session_creation: Arc<Mutex<HashMap<PathBuf, Arc<CreationSlot>>>>`.

`get_component_session(build_dir)`:

```mermaid
sequenceDiagram
    participant Tool
    participant WS as WorkspaceSession
    participant Slot as CreationSlot
    participant CS as ComponentSession
    Tool->>WS: get_component_session(build_dir)
    WS->>WS: check cache (short lock)
    alt cached
        WS-->>Tool: Arc ComponentSession
    else in-flight
        WS->>Slot: await_result
        Slot-->>Tool: shared result
    else new
        WS->>WS: register CreationSlot
        WS->>WS: discover component if not in workspace
        WS->>CS: ComponentSession::new
        CS->>CS: spawn clangd, init LSP, start index monitor
        WS->>WS: insert into cache
        WS->>Slot: store_result, notify_waiters
        WS-->>Tool: Arc ComponentSession
    end
```

The cache lock is held only for the lookup, never across clangd startup, to avoid deadlocks. The `CreationSlot` uses a `tokio::sync::Notify`; the creator stores the result and notifies, concurrent waiters receive the same `Arc<ComponentSession>`.

### ComponentSession

`ComponentSession` (`src/project/component_session.rs`) owns all resources for one build directory:

| Field | Role |
|---|---|
| `build_dir: PathBuf` | The build directory this session represents |
| `clangd_session: Arc<Mutex<ClangdSession>>` | The clangd process + LSP client |
| `file_manager: Arc<Mutex<ClangdFileManager>>` | LSP document sync state |
| `index_monitor: Arc<ComponentIndexMonitor>` | Index state tracking + completion latch |
| `component: ProjectComponent` | Metadata (build type, generator, paths) |
| `diagnostics_collector: Arc<DiagnosticsCollector>` | Captured `publishDiagnostics` |

Construction (`ComponentSession::new`) is async and does the real work:
1. Load the `CompilationDatabase` from `component.compilation_database_path`.
2. Build a `ClangdConfig` via `ClangdConfigBuilder` - working directory is the project root, build directory is the component's build dir, plus `--limit-results` and `--log=verbose` args.
3. Create a progress-event channel (`PROGRESS_CHANNEL_BUFFER_SIZE = 10_000`).
4. `ClangdSessionBuilder::new().with_config(config).with_progress_sender(tx).build().await` spawns clangd and initializes LSP.
5. Register the `DiagnosticsCollector` notification handler on the LSP client.
6. Create the `ClangdFileManager`.
7. Create the `ComponentIndexMonitor` (which builds a `ComponentIndex` from the compilation database and clangd version, and wires a `ClangdIndexTrigger`).
8. Spawn a background task that drains `progress_rx` into `monitor.handle_progress_event`.

The session exposes accessors (`get_index_status`, `component()`, `clangd_session`, `file_manager`, `diagnostics_collector`, `index_monitor`) used by the tools.

## Error model

`ProjectError` (`src/project/error.rs`) covers path/validation/parse/IO/session-creation failures. Notable variants: `BuildDirectoryNotReadable`, `SourceRootNotFound`, `CompilationDatabaseNotFound`, `InvalidBuildDirectory`, `PathNotFound`, `ParseError`, `SessionCreation(String)`. Tool handlers map these to `CallToolError`.
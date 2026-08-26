# C++ MCP Server

A Model Context Protocol (MCP) server that provides C++ code analysis capabilities through clangd LSP integration. Enables AI agents to work with C++ codebases using semantic understanding similar to modern IDEs.

This is a fork of [mpsm/mcp-cpp](https://github.com/mpsm/mcp-cpp).

## Why This MCP Server?

Modern C++ development relies heavily on advanced tooling to navigate complex codebases with preprocessor macros, template instantiations, and intricate inheritance hierarchies. While humans use IntelliSense-powered IDEs to understand these complexities, most AI agents rely on text-only browsing.

This MCP server bridges that gap by providing AI agents with semantic analysis capabilities similar to what developers experience in modern IDEs. Unlike generic LSP MCP implementations, this server focuses specifically on C++ workflows.

The server can handle multiple C++ projects simultaneously, which is particularly useful for complex scenarios like embedded Linux development where understanding interactions between individual components is crucial. It supports both CMake and Meson build systems with automatic build directory detection and switching.


## Features

The server provides five core analysis tools for C++ development. The `get_project_details` tool performs dynamic CMake and Meson build environment discovery and reports the configuration it resolved. `get_index_status` forwards clangd's reported indexing state. For symbol exploration, `search_symbols` offers C++ symbol search with project boundary detection and filtering. For deeper analysis, `analyze_symbol_context` provides inheritance and call hierarchy support. For code health, `show_diagnostics` retrieves clangd's compile errors, warnings, and notes for a specific source file.

The implementation works with both CMake and Meson projects, handles multiple libraries and executables, automatically discovers build configurations, and includes a Python CLI for symbol exploration.

## Component Discovery

The MCP server automatically looks for components in the current working directory, scanning 3 levels below by default. This scan depth can be changed using tool options. When an AI agent requests analysis using a build directory outside the project, the MCP server will use that hint path and create a component from it, allowing flexible project analysis beyond the default scanning scope.

## Repository Structure

This fork follows the layout below:

```
├── src/                    # Rust server
│   ├── clangd/             #   clangd LSP integration (session, config, index)
│   ├── lsp/                #   LSP protocol client (JSON-RPC, framing)
│   ├── project/            #   build-system discovery (CMake, Meson, compile_commands)
│   ├── mcp_server/         #   MCP tools (search, analyze, diagnostics, index status)
│   └── io/                 #   process and transport management
├── tools/
│   └── lsp-cli.py          # standalone Python CLI (stdlib only, no deps)
├── test/                   # test projects + Node.js E2E framework
├── docs/                   # design notes (clangd index spec, symbol analysis)
├── docker/ + Dockerfile    # container packaging
└── .github/workflows/      # CI + release (pre-built binaries per platform)
```

## Dependencies

The server requires clangd 11 or later for C++ semantic analysis (clangd 20+ recommended). Your project must generate a compilation database (`compile_commands.json`): CMake and Meson are auto-detected, and any other build system that exports one (GN/ninja, Bazel, ...) works through the project-root `compile_commands.json` fallback. The Python CLI needs only the Python 3 standard library.

You can optionally set the `CLANGD_PATH` environment variable to specify a custom clangd binary location.

## Installation

The server is a single self-contained binary. You do **not** need Rust or cargo on the machine that runs it — build it once (or download a pre-built binary) and copy it wherever you need it.

### Download a Pre-Built Binary (no cargo needed)

Pre-built binaries for Linux (x86_64 / aarch64) and macOS (x86_64 / aarch64) are attached to each [GitHub release](https://github.com/vptyp/mcp-cpp/releases):

```bash
# Example: Linux x86_64
curl -L -o mcp-cpp-server \
  https://github.com/vptyp/mcp-cpp/releases/latest/download/mcp-cpp-server-linux-x86_64
chmod +x mcp-cpp-server
./mcp-cpp-server --help
```

### Build Directly from Source

Build on any machine with cargo, then run the binary directly — no install step, no cargo on the target machine:

```bash
# Clone the fork
git clone git@github.com:vptyp/mcp-cpp.git
cd mcp-cpp

# Compile the release binary
cargo build --release

# The binary is self-contained; copy it to machines without cargo:
./target/release/mcp-cpp-server --help
```

### Docker Installation

```bash
# Build the Docker image
docker build -t mcp-cpp-server .

# Run with your C++ project mounted
docker run -i --rm -v /path/to/your/cpp-project:/workspace mcp-cpp-server
```

The Docker image includes:
- mcp-cpp-server binary
- clangd-20 for C++ semantic analysis
- Minimal Ubuntu-based runtime

## Usage

### Claude CLI Integration (Tested)

For Claude CLI, create or update your MCP configuration file (`~/.config/claude-cli/mcp_servers.json`):

```json
{
  "mcpServers": {
    "cpp-tools": {
      "command": "mcp-cpp-server",
      "env": {
        "CLANGD_PATH": "/usr/bin/clangd-20"
      }
    }
  }
}
```

### Amazon Q Developer CLI Integration (Tested)

For Amazon Q Developer CLI, add to your MCP configuration:

```json
{
  "mcpServers": {
    "cpp-tools": {
      "command": "mcp-cpp-server",
      "env": {
        "CLANGD_PATH": "/usr/bin/clangd-20"
      }
    }
  }
}
```

### Claude Desktop Integration

Add to your Claude Desktop configuration file (`~/.claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "cpp-tools": {
      "command": "mcp-cpp-server",
      "env": {
        "CLANGD_PATH": "/usr/bin/clangd-20"
      }
    }
  }
}
```

### Claude Code Integration (VS Code Extension)

Claude Code uses a different configuration file location and requires explicit permissions. Add to your Claude Code configuration file (`~/.claude.json`):

```json
{
  "mcpServers": {
    "cpp": {
      "type": "stdio",
      "command": "/path/to/mcp-cpp/target/release/mcp-cpp-server",
      "args": [],
      "env": {
        "CLANGD_PATH": "/opt/homebrew/opt/llvm/bin/clangd"
      }
    }
  },
  "permissions": {
    "allow": [
      "mcp__cpp__search_symbols",
      "mcp__cpp__analyze_symbol_context",
      "mcp__cpp__get_project_details",
      "mcp__cpp__show_diagnostics"
    ]
  }
}
```

**Notes:**
- Claude Code reads `~/.claude.json`, not `~/.claude/mcp_servers.json`
- The `permissions` section is required to enable the MCP tools
- Adjust the `command` path to wherever you placed the binary (e.g. `target/release/mcp-cpp-server` after building, or a downloaded release binary)
- Adjust `CLANGD_PATH` to your clangd installation (use `which clangd` to find it, or omit if clangd is in your PATH)
- Tools are prefixed with `mcp__cpp__` in Claude Code (e.g., `mcp__cpp__search_symbols`)

### Docker Usage with MCP Clients

**Claude Desktop/CLI:**

```json
{
  "mcpServers": {
    "cpp-tools": {
      "command": "docker",
      "args": [
        "run", "-i", "--rm",
        "-v", "/path/to/cpp-project:/workspace",
        "mcp-cpp-server",
        "--root", "/workspace"
      ]
    }
  }
}
```

**Note:** Arguments after the image name are passed to mcp-cpp-server. Use `-e RUST_LOG=debug` for verbose logging.

## Platform Support

Tested on:

- **Windows with WSL2 Ubuntu**
- **Ubuntu (native)**
- **macOS**

## Configuration

### CLI Options

```bash
mcp-cpp-server --help

# Options:
--root <DIR>             Project root directory to scan for build configurations (defaults to current directory)
--clangd-path <PATH>     Path to clangd executable (overrides CLANGD_PATH env var and .mcp-cpp.yaml)
--log-level <LEVEL>      Log level (overrides RUST_LOG env var)
--log-file <FILE>        Log file path (overrides MCP_LOG_FILE env var)
--transport <TRANSPORT>  Transport to serve on: stdio (default) or http (streamable HTTP)
--host <HOST>            Host to bind the streamable-http server to (default: 127.0.0.1)
--port <PORT>            Port to bind the streamable-http server to (default: 8080)
```

### Project Configuration File (`.mcp-cpp.yaml`)

Settings that belong to a project rather than to a machine go in a
`.mcp-cpp.yaml` at the project root. It is optional — with no file the server
uses the defaults below. Every key is optional, and unknown keys are rejected
with an error naming the offending line, so a typo never silently does nothing.

```yaml
version: 1

clangd:
  path: clangd                  # binary to run
  args: []                      # extra flags, appended last so they win
  background_index: true        # --background-index
  pch_storage: memory           # memory | disk
  index_threads: null           # -j N; null means one per core
  workspace_symbol_limit: 1000  # --limit-results
  initialization_timeout: 30s   # wait for clangd to answer `initialize`
  request_timeout: 30s          # bound on every LSP request
  index_wait_timeout: 20s       # bound on clangd index-progress waits

project:
  scan_depth: 3                 # how deep to look for build directories
  skip_hidden: true             # ignore dot-directories while scanning
  follow_symlinks: false
  max_components: null          # null means no cap

server:
  host: 127.0.0.1               # streamable-http bind address
  port: 8080
```

Durations accept human-readable strings (`500ms`, `30s`, `2m`, `1h`).

**Precedence**, highest first: command-line arguments → environment variables →
`.mcp-cpp.yaml` → built-in defaults. A missing file is normal; a malformed one
is fatal, because silently ignoring settings you deliberately wrote is worse
than refusing to start.

The `get_project_details` tool reports the configuration the server actually
resolved, along with the file it came from, so you can confirm your settings
were picked up before blaming clangd.

`host` defaults to loopback on purpose: this server exposes a project's entire
source tree, so binding it to a routable address should be a deliberate act.

### Environment Variables

- **`CLANGD_PATH`**: Path to clangd executable (default: "clangd")
- **`RUST_LOG`**: Log level - trace, debug, info, warn, error (default: "info")
- **`MCP_LOG_FILE`**: Path to log file (default: logs to stderr only)
- **`MCP_LOG_UNIQUE`**: Set to "true" to append process ID to log filename

### Python CLI for Debugging

The Python CLI helps you understand what your AI agent sees from the MCP server, making it useful for debugging interactions. It needs only the Python standard library. Note that this tool is not included in the distributed package and must be used directly from the repository:

```bash
# Clone the fork if you haven't already
git clone git@github.com:vptyp/mcp-cpp.git
cd mcp-cpp

# Discover build directories and see the resolved configuration
python3 tools/lsp-cli.py project

# Search for symbols (see what the agent would see)
python3 tools/lsp-cli.py search MyClass

# Get complete API overview of a header file
python3 tools/lsp-cli.py search "" --files include/api.h

# Analyze a symbol with examples
python3 tools/lsp-cli.py analyze MyClass::process

# Show clangd diagnostics (errors/warnings) for one or more source files
python3 tools/lsp-cli.py diagnostics src/main.cpp src/util.cpp

# Machine-readable output for scripting
python3 tools/lsp-cli.py --format json search MyClass
```

Output is YAML by default; `--format json` emits the tool result as JSON and
`--format raw` the untouched JSON-RPC response. Each command also has a long
alias matching its MCP tool name (`search-symbols`, `get-project-details`, …),
and `--help` on any command documents its options.

### Basic Workflow

1. **Get Project Details**

   ```json
   { "name": "get_project_details" }
   ```

   With custom scan parameters:

   ```json
   {
     "name": "get_project_details",
     "arguments": {
       "path": "/path/to/project",
       "depth": 5
     }
   }
   ```

2. **Search C++ Symbols**

   ```json
   {
     "name": "search_symbols",
     "arguments": { "query": "std::vector", "include_external": true }
   }
   ```

   File-specific search with custom build directory:

   ```json
   {
     "name": "search_symbols",
     "arguments": {
       "query": "MyClass",
       "files": ["include/MyClass.hpp"],
       "build_directory": "build-debug",
       "wait_timeout": 30
     }
   }
   ```

3. **Analyze Symbol Context**

   ```json
   {
     "name": "analyze_symbol_context",
     "arguments": {
       "symbol": "MyClass::process",
       "max_examples": 5
     }
   }
   ```

   With location hint for disambiguation:

   ```json
   {
     "name": "analyze_symbol_context",
     "arguments": {
       "symbol": "factorial",
       "build_directory": "/path/to/build",
       "location_hint": "/path/to/file.cpp:42:15",
       "wait_timeout": 0
     }
   }
   ```

4. **Show File Diagnostics**

   ```json
   {
     "name": "show_diagnostics",
     "arguments": { "file": "src/main.cpp" }
   }
   ```

   With a custom build directory and timeout:

   ```json
   {
     "name": "show_diagnostics",
     "arguments": {
       "file": "src/main.cpp",
       "build_directory": "/path/to/build",
       "wait_timeout": 30
     }
   }
   ```

## Use Cases

The server excels at code exploration and navigation, helping you find functions, classes, and variables across large codebases. It can analyze relationships between code components and navigate system libraries and third-party dependencies to understand how different parts of your project interact.

For code analysis and review, the server provides detailed symbol context including usage patterns, inheritance relationships, and call hierarchies. This helps you explore class hierarchies and call patterns, making it easier to understand unfamiliar code or prepare for refactoring by identifying all usages and dependencies. It can also surface clangd's compile errors and warnings for a file, helping you catch broken or risky code before it reaches a build.

The server also assists with development workflows by enabling switching between Debug, Release, and custom build configurations. It provides clear separation between project symbols and external library symbols, making navigation through large C++ codebases more efficient. The cross-reference generation helps you find all references, implementations, and related symbols quickly.

## Tool Reference

### C++ Analysis Tools

#### `get_project_details`

**Purpose**: Multi-provider build system analysis and project workspace discovery

**Options**:
- `path` (optional): Project root path to scan. If different from server default, triggers fresh scan
- `depth` (optional): Scan depth for component discovery (0-10 levels). Controls how many directory levels below the project root to search for CMake/Meson components. Defaults to `project.scan_depth` from `.mcp-cpp.yaml`, which is 3 if unset
- `include_details` (optional): Include the full set of build options rather than just a count (default: false)

**Output**: Complete project analysis including build configurations, components, compilation database status, multi-provider discovery (CMake, Meson, etc.), and a `configuration` block reporting the effective settings and the config file they came from

**Component Discovery**: By default, scans 3 levels below the project root for components. When AI agents specify build directories outside this scope, the server creates components from those hint paths automatically.

#### `search_symbols`

**Purpose**: Find C++ symbols across your codebase or get complete API overviews

**Key Capabilities**:

- **Symbol Discovery**: Find functions, classes, variables by name or pattern
- **Complete File Overview**: Use empty query (`""`) with file parameter to list all symbols in any file
- **API Exploration**: Perfect for understanding unfamiliar headers or source files
- **Smart Filtering**: Filter by symbol types (Class, Function, Method, etc.) and exclude external libraries

**Common Use Cases**:

```bash
# Find all vector-related symbols
search_symbols {"query": "vector"}

# Get complete overview of a header file
search_symbols {"query": "", "files": ["include/api.h"]}

# Find only classes and structs
search_symbols {"query": "Process", "kinds": ["Class", "Struct"]}
```

#### `analyze_symbol_context`

**Purpose**: Deep dive analysis of any C++ symbol with comprehensive context

**What You Get**:

- **Symbol Definition**: Complete type information, location, documentation
- **Usage Examples**: Real code showing how the symbol is used
- **Class Members**: All methods, fields, constructors (for classes)
- **Inheritance Tree**: Base classes and derived classes (for classes)
- **Call Relationships**: What calls this function and what it calls (for functions)

**Perfect For**:

- Understanding unfamiliar code
- Finding all usages before refactoring
- Exploring class hierarchies and relationships
- Learning how to use a function or class

```bash
# Analyze a class and its members
analyze_symbol_context {"symbol": "MyClass"}

# Deep dive into a specific method
analyze_symbol_context {"symbol": "MyClass::process", "max_examples": 3}
```

#### `show_diagnostics`

**Purpose**: Retrieve clangd's semantic diagnostics (compile errors, warnings, notes) for a single source file

**What You Get**:

- **Diagnostics**: Errors, warnings, and info/hint diagnostics with exact source ranges (line/character)
- **Severity Counts**: Summary of errors, warnings, and notes for quick triage
- **Clean-File Detection**: Distinguishes a clean file from one clangd hasn't parsed yet
- **Related Information**: Optional related diagnostics (e.g. notes pointing at the root cause)

**How It Works**: LSP diagnostics are *pushed* by clangd only after a file is opened in the session. The tool opens the target file (triggering a fresh parse), captures the `textDocument/publishDiagnostics` notification, and returns the result.

**Perfect For**:

- Verifying a file compiles cleanly before committing
- Finding the exact compile errors clangd reports for a broken file
- Understanding warnings that may indicate subtle bugs or undefined behavior

```bash
# Check a file for errors/warnings
show_diagnostics {"file": "src/main.cpp"}

# With an explicit build directory and longer wait
show_diagnostics {"file": "src/main.cpp", "build_directory": "/path/to/build", "wait_timeout": 30}
```

**Options**:
- `file` (required): Path to the C++ source file to analyze (relative paths resolved against the project root)
- `build_directory` (optional): Build directory containing `compile_commands.json` (auto-detected when only one exists)
- `wait_timeout` (optional): Seconds to wait for clangd to publish diagnostics (default: 20, 0 = no wait)

## Limitations

- Requires CMake or Meson projects that generate `compile_commands.json`

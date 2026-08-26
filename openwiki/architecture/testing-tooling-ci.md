---
type: Reference
title: Testing, Tooling, and CI
description: Rust unit/integration tests, TypeScript E2E suite, Python CLI tools, Docker image, GitHub Actions CI/release workflows, and repository docs.
resource: https://github.com/mpsm/mcp-cpp
tags: [testing, ci, docker, python, tooling, release]
openwiki:
  roles: [testing, operations, delivery]
  source_paths:
    - Cargo.toml
    - Dockerfile
    - docker/entrypoint.sh
    - tools/lsp-cli.py
    - tools/read-cmake-cache.py
    - test/test-project/CMakeLists.txt
    - test/test-meson-project/meson.build
  invariants:
    - clangd-integration-tests, test-logging, and project-integration-tests are opt-in Cargo features (all default-off)
    - CI installs clangd-20 and cmake/meson/ninja for the integration test jobs
    - The Docker image bundles clangd-20 and serves stdio JSON-RPC on /workspace
  validation_commands:
    - cargo test --verbose
    - cargo test --features clangd-integration-tests,test-logging clangd
    - cargo test --features project-integration-tests,test-logging project
    - cargo fmt --all -- --check
    - cargo clippy --all-targets --all-features -- -D warnings
---

# Testing, Tooling, and CI

## Rust tests

The crate uses three opt-in Cargo features (all default-off) to gate tests that need external tools:

| Feature | Requires | CI job |
|---|---|---|
| `clangd-integration-tests` | a real `clangd` binary (`CLANGD_PATH`) | `rust-integration-tests` |
| `project-integration-tests` | `cmake` and `meson` installed | `project-integration-tests` |
| `test-logging` | (none) - enables `tracing-subscriber` in tests | paired with the above |

Dev-dependencies: `tempfile` (temp fixtures), `ctor` (test init), `mockall` (mock trait generation - `MockLspClientTrait`, `MockProcessManager`, `MockTransport`, `MockFileSystemTrait`). Unit tests run with `cargo test --verbose`; integration tests target the `clangd` and `project` module paths with the relevant features.

`src/clangd/testing.rs` provides `MockClangdSession`, `create_test_config`, `create_mock_session`, `create_session_with_mock_dependencies`, and `create_integration_test_session` (feature-gated). `src/test_utils.rs` contains shared test fixtures.

## TypeScript E2E suite (`test/e2e`)

A Node.js/Vitest framework that exercises the real built server binary against real C++ projects with clangd-20. Uses the official `@modelcontextprotocol/sdk` as the MCP client, `ajv` for JSON schema validation, and `fs-extra` for fixture setup.

- **Framework** (`src/framework/`): `McpClient.ts` (JSON-RPC client wrapper), `TestProject.ts` (sets up/tears down a C++ project copy), `TestHelpers.ts`/`TestUtils.ts`.
- **Suites** (`src/tests/`): `search-symbols.test.ts`, `analyze-symbol-context.test.ts`, `get-project-details.test.ts`, `example-with-context.test.ts`.
- **Scripts**: `run-tests.sh` (full pipeline), `inspect-test-dirs.ts`, `cleanup-test-folders.ts`.
- **npm scripts**: `test`, `test:e2e`, `test:framework`, `test:full`, `lint`, `format`, `validate`.

## Fixtures

| Fixture | Build system | Contents |
|---|---|---|
| `test/test-project/` | CMake, C++20 | `TestProject` + `TestLib` static lib; 10 headers + 7 sources; inheritance hierarchies, storage backends, templates, enum operators. `CMAKE_EXPORT_COMPILE_COMMANDS ON`. |
| `test/test-meson-project/` | Meson, C++17 | `test_meson_app` exe + `test_meson_lib` static lib; `math.h`/`utils.h` + 3 sources. |
| `test/requests/` | - | 11 raw JSON-RPC request payloads for replay testing (`tools/list`, `search_symbols`, `analyze_symbol_context`, call hierarchy, inheritance). |

## Python tools (`tools/`)

Only runtime dep: `rich>=13.0.0` (optional - tools fall back to plain output if missing).

| Tool | Purpose | Usage |
|---|---|---|
| `lsp-cli.py` | MCP server CLI client with rich output. Supports three transports (spawn, attach via FIFO, streamable-HTTP with SSE + session caching). | `get-index-status`, `search-symbols`, `search-class`, `search-method`, `show-diagnostics`, `analyze-symbol`, `get-project-details` |
| `read-cmake-cache.py` | ccmake-like viewer for `CMakeCache.txt` showing only user-configurable, non-advanced entries. | `read-cmake-cache.py [path/to/CMakeCache.txt]` |

`lsp-cli.py` is the primary debugging tool - it shows exactly what an AI agent would see from the server. It maintains a per-project `.lsp-cli.json` cache for transport/session auto-reconnect.

## Docker

Multi-stage `Dockerfile`: stage 1 builds the release binary with `rust:1.89`; stage 2 is `ubuntu:24.04` with `clangd-20` from the LLVM APT repo, copies the binary to `/usr/local/bin/mcp-cpp-server`, sets `CLANGD_PATH=/usr/bin/clangd-20` and `RUST_LOG=info`, workdir `/workspace`. `docker/entrypoint.sh` validates clangd exists, warns if interactive or if `/workspace` is empty, then `exec`s the server. Default CMD is `["--root", "/workspace"]`.

## GitHub Actions

### `ci.yml` (push/PR to main)

| Job | What |
|---|---|
| `rust-format` | `cargo fmt --all -- --check` |
| `rust-clippy` | `cargo clippy --all-targets --all-features -- -D warnings` |
| `rust-test` | `cargo test --verbose` (unit tests) |
| `rust-build` | `cargo build --verbose` |
| `cross-platform-build` | Matrix ubuntu/windows/macos -> `cargo build --release` |
| `rust-integration-tests` | Installs clangd-20 + cmake, `cargo test --features clangd-integration-tests,test-logging clangd` |
| `project-integration-tests` | Installs cmake/meson/ninja, `cargo test --features project-integration-tests,test-logging project` |
| `cpp-build` | Configures + builds both CMake and Meson fixtures |
| `ts-format-lint` | In `test/e2e`: `npm ci`, `format:check`, `lint` |

### `release.yml` (on `v*` tags)

Staged pipeline: `rust-checks` (fmt + clippy + tests + build + `cargo package --no-verify` + `cargo audit`) + `cpp-checks` + `ts-checks` -> `e2e-integration` (runs the TS E2E suite against the cached release binary with clangd-20) -> `version-check` (tag vs Cargo.toml) -> `build-binaries` (matrix x86_64/aarch64 linux/darwin) -> `create-github-release` -> `publish-crates` (`cargo publish` to crates.io).

### `openwiki-update.yml`

Scheduled `cron: "0 8 * * *"` plus `workflow_dispatch`. Sets up Node 22, installs `openwiki` + `mermaid` + `jsdom`, runs `openwiki code --update --print`, and creates a PR on branch `openwiki/update`.

## Repository docs (`docs/`)

| Doc | Content |
|---|---|
| `docs/symbol_context_analyzer_implementation.md` | Implementation plan for `analyze_symbol_context`: input/output schema, LSP call aggregation. |
| `docs/symbol_search_explorer_implementation.md` | Implementation plan for `search_symbols`: workspace scope, visibility rules, fuzzy matching. |

## Agent guidance files

- `AGENTS.md` - OpenWiki-scoped directive: source/tests are authoritative, prefer narrowest validation, do not hand-edit generated wiki pages.
- `CLAUDE.md` - full project context, architecture map, and "Technical Peer Collaboration Mode" guidance.
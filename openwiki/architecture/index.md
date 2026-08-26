# Files

- [Clangd Session and Indexing](clangd-session.md) - The clangd session lifecycle, LSP client/protocol/framing layers, and the dual-source indexing subsystem that tracks clangd progress via LSP notifications and stderr log parsing.
- [Architecture](overview.md) - Layered architecture of mcp-cpp-server from MCP handler down through project/workspace sessions, clangd sessions, the LSP client, and the IO layer.
- [Project and Workspace Subsystem](project-workspace.md) - Project scanning, multi-provider build discovery (CMake/Meson), and the WorkspaceSession/ComponentSession lifecycle that manages per-build-directory clangd processes.
- [Testing, Tooling, and CI](testing-tooling-ci.md) - Rust unit/integration tests, TypeScript E2E suite, Python CLI tools, Docker image, GitHub Actions CI/release workflows, and repository docs.

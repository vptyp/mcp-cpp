//! Workspace session management
//!
//! Provides `WorkspaceSession` for managing ComponentSession instances across different
//! build directories within a project workspace. This module handles pure session
//! lifecycle management without build directory resolution policy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use crate::clangd::version::ClangdVersion;
use crate::config::AppConfig;
use crate::project::compilation_database::CompilationDatabase;
use crate::project::component::ProjectComponent;
use crate::project::component_session::ComponentSession;
use crate::project::{ProjectError, ProjectScanner, ProjectWorkspace};

/// Manages ComponentSession instances for a project workspace
///
/// `WorkspaceSession` provides pure session lifecycle management, handling the creation,
/// reuse, and cleanup of ComponentSession instances for different build directories.
/// This orchestrates component sessions while maintaining the same external API.
/// Supports dynamic component discovery for build directories not found in initial scanning.
pub struct WorkspaceSession {
    /// Project workspace for determining project root and components (mutable for dynamic discovery)
    workspace: Arc<Mutex<ProjectWorkspace>>,
    /// Map of build directories to their ComponentSession instances
    component_sessions: Arc<Mutex<HashMap<PathBuf, Arc<ComponentSession>>>>,
    /// In-flight session creations: build dir -> shared slot that the single creator
    /// fills (with the resulting session or an error) and notifies. Concurrent first
    /// requests await the same slot so they share one session/clangd instead of
    /// spawning duplicates.
    session_creation: Arc<Mutex<HashMap<PathBuf, Arc<CreationSlot>>>>,
    /// Resolved application configuration (clangd settings, scan settings, ...)
    config: Arc<AppConfig>,
    /// Clangd version information
    clangd_version: ClangdVersion,
    /// Project scanner for dynamic component discovery
    scanner: ProjectScanner,
}

/// Shared slot for deduplicating in-flight session creation. The single creator
/// stores the outcome and notifies all waiters.
struct CreationSlot {
    notify: tokio::sync::Notify,
    result: std::sync::Mutex<Option<Result<Arc<ComponentSession>, String>>>,
}

impl CreationSlot {
    fn new() -> Self {
        Self {
            notify: tokio::sync::Notify::new(),
            result: std::sync::Mutex::new(None),
        }
    }

    fn store_result(&self, result: Option<Result<Arc<ComponentSession>, String>>) {
        *self.result.lock().unwrap() = result;
        self.notify.notify_waiters();
    }

    async fn await_result(&self) -> Option<Result<Arc<ComponentSession>, String>> {
        // Return immediately if the result is already stored.
        if let Some(r) = self.result.lock().unwrap().as_ref() {
            return Some(r.clone());
        }
        loop {
            self.notify.notified().await;
            if let Some(r) = self.result.lock().unwrap().as_ref() {
                return Some(r.clone());
            }
        }
    }
}

impl WorkspaceSession {
    /// Create a new WorkspaceSession for the given project workspace
    pub fn new(workspace: ProjectWorkspace, config: Arc<AppConfig>) -> Result<Self, ProjectError> {
        // Detect clangd version for index format compatibility
        let clangd_version =
            ClangdVersion::detect(Path::new(&config.clangd.path)).map_err(|e| {
                ProjectError::SessionCreation(format!("Failed to detect clangd version: {}", e))
            })?;

        info!(
            "Detected clangd version: {}.{}.{}",
            clangd_version.major, clangd_version.minor, clangd_version.patch
        );

        // Create scanner with default providers for dynamic discovery
        let scanner = ProjectScanner::with_default_providers();

        Ok(Self {
            workspace: Arc::new(Mutex::new(workspace)),
            component_sessions: Arc::new(Mutex::new(HashMap::new())),
            session_creation: Arc::new(Mutex::new(HashMap::new())),
            config,
            clangd_version,
            scanner,
        })
    }

    /// Get or create a ComponentSession for the specified build directory
    pub async fn get_component_session(
        &self,
        build_dir: PathBuf,
    ) -> Result<Arc<ComponentSession>, ProjectError> {
        // Fast path: reuse an existing session. The component_sessions lock is only
        // held for the cache lookup, never across clangd startup, to avoid deadlocks
        // when concurrent requests overlap.
        {
            let sessions = self.component_sessions.lock().await;
            if let Some(component_session) = sessions.get(&build_dir) {
                info!(
                    "Reusing existing ComponentSession for build dir: {}",
                    build_dir.display()
                );
                return Ok(Arc::clone(component_session));
            }
        }

        // In-flight dedup: if another request is already creating this session, wait
        // for it rather than spawning a second clangd. The creator fills the shared
        // slot and notifies when it finishes.
        let existing = {
            let pending = self.session_creation.lock().await;
            pending.get(&build_dir).cloned()
        };
        if let Some(slot) = existing {
            info!(
                "Waiting for in-flight session creation for build dir: {}",
                build_dir.display()
            );
            if let Some(session) = slot.await_result().await {
                return session.map_err(ProjectError::SessionCreation);
            }
            // Creator ended without a result; fall through to create our own.
        }

        // Register ourselves as the creator so concurrent requests wait on us.
        let slot = Arc::new(CreationSlot::new());
        {
            let mut pending = self.session_creation.lock().await;
            if let Some(existing) = pending.get(&build_dir).cloned() {
                // Lost the race: someone registered between our check and insert.
                drop(pending);
                if let Some(session) = existing.await_result().await {
                    return session.map_err(ProjectError::SessionCreation);
                }
                // Fall back to creating our own if the winner produced nothing.
                return self.create_component_session(&build_dir).await;
            }
            pending.insert(build_dir.clone(), Arc::clone(&slot));
        }

        // Perform the actual (now fast) session creation outside any creation lock.
        let result = self.create_component_session(&build_dir).await;

        // Publish the result and remove our in-flight registration.
        // Publish the result and remove our in-flight registration.
        let published = match &result {
            Ok(session) => Some(Ok(Arc::clone(session))),
            Err(e) => Some(Err(e.to_string())),
        };
        slot.store_result(published);
        {
            let mut pending = self.session_creation.lock().await;
            pending.remove(&build_dir);
        }

        result
    }

    /// Create (and cache) a ComponentSession for a build directory.
    /// Assumes the caller has already checked the cache and registered as creator.
    async fn create_component_session(
        &self,
        build_dir: &PathBuf,
    ) -> Result<Arc<ComponentSession>, ProjectError> {
        // Create a new component session for this build directory
        info!(
            "Creating new ComponentSession for build dir: {}",
            build_dir.display()
        );

        // Try to get the component from the workspace first
        let component = {
            let workspace = self.workspace.lock().await;
            workspace.get_component_by_build_dir(build_dir).cloned()
        };

        let component = match component {
            Some(comp) => comp,
            None => {
                // Component not found in workspace - try dynamic discovery
                info!(
                    "Component not found in workspace, attempting dynamic discovery for: {}",
                    build_dir.display()
                );

                match self.scanner.discover_component(build_dir)? {
                    Some(discovered_component) => {
                        // Add the discovered component to the workspace
                        let mut workspace = self.workspace.lock().await;
                        workspace.add_component(discovered_component.clone());
                        info!(
                            "Successfully discovered and added component for build dir: {}",
                            build_dir.display()
                        );
                        discovered_component
                    }
                    None => {
                        // Provider discovery found nothing - fall back to synthesizing a
                        // component directly from a bare compile_commands.json. This supports
                        // build systems (GN, Bazel, xmake, ...) that export a compilation
                        // database but aren't recognized by any provider.
                        match synthesize_component_from_compile_commands(build_dir) {
                            Some(component) => {
                                let mut workspace = self.workspace.lock().await;
                                workspace.add_component(component.clone());
                                info!(
                                    "Synthesized component from compile_commands.json for build dir: {}",
                                    build_dir.display()
                                );
                                component
                            }
                            None => {
                                let workspace = self.workspace.lock().await;
                                let available_dirs = workspace.get_build_dirs();
                                return Err(ProjectError::SessionCreation(format!(
                                    "No valid project component found at build directory: '{}'. Scan root: '{}'. Use get_project_details to discover available build directories. Available directories: {:?}. Ensure you're using absolute paths from that output to avoid path concatenation issues.",
                                    build_dir.display(),
                                    workspace.project_root_path.display(),
                                    available_dirs
                                )));
                            }
                        }
                    }
                }
            }
        };

        // Determine project root from workspace
        let project_root = {
            let workspace = self.workspace.lock().await;
            if workspace.project_root_path.exists() {
                workspace.project_root_path.clone()
            } else {
                std::env::current_dir().map_err(|e| {
                    ProjectError::SessionCreation(format!("Failed to get current directory: {}", e))
                })?
            }
        };

        let component_session =
            ComponentSession::new(component, &self.config, &self.clangd_version, project_root)
                .await?;

        let component_session_arc = Arc::new(component_session);

        // Insert under the lock. If a concurrent request created a session for the
        // same build dir while we were initializing ours, prefer the existing one
        // and drop our duplicate.
        let mut sessions = self.component_sessions.lock().await;
        if let Some(existing) = sessions.get(build_dir) {
            Ok(Arc::clone(existing))
        } else {
            sessions.insert(build_dir.clone(), Arc::clone(&component_session_arc));
            Ok(component_session_arc)
        }
    }

    /// Get a non-mutable reference to the project workspace
    ///
    /// Note: This now returns an Arc<Mutex<ProjectWorkspace>> since the workspace
    /// can be mutated during dynamic component discovery. Callers should lock
    /// the mutex to access workspace data.
    pub fn get_workspace(&self) -> &Arc<Mutex<ProjectWorkspace>> {
        &self.workspace
    }

    /// The resolved application configuration backing this session
    pub fn config(&self) -> &Arc<AppConfig> {
        &self.config
    }
}

/// Synthesize a `ProjectComponent` directly from a bare `compile_commands.json`
/// in the given build directory.
///
/// This is the fallback used when no provider recognizes the build directory. It
/// anchors the component on the build directory (which clangd needs via
/// `--compile-commands-dir`) and derives the source root from the common
/// ancestor of the source file paths in the database, rather than assuming the
/// build directory is the source root.
///
/// Returns `None` if the directory has no `compile_commands.json`, the database
/// can't be parsed, or the derived component fails validation.
fn synthesize_component_from_compile_commands(build_dir: &Path) -> Option<ProjectComponent> {
    let db_path = build_dir.join("compile_commands.json");
    if !db_path.exists() {
        return None;
    }

    let db = CompilationDatabase::new(db_path.clone()).ok()?;
    let source_root = db.derive_source_root().ok()?;

    ProjectComponent::new(
        build_dir.to_path_buf(),
        source_root,
        db_path,
        "compile_commands".to_string(),
        String::new(),
        String::new(),
        HashMap::new(),
    )
    .ok()
}

impl Drop for WorkspaceSession {
    fn drop(&mut self) {
        // Clear the component sessions HashMap to drop all Arc references
        // ComponentSession::drop() will be called for proper cleanup of resources
        if let Ok(mut sessions) = self.component_sessions.try_lock() {
            sessions.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "clangd-integration-tests")]
    use super::*;
    #[cfg(feature = "clangd-integration-tests")]
    use crate::test_utils::integration::TestProject;
    #[cfg(feature = "clangd-integration-tests")]
    use std::sync::Arc;

    // Auto-initialize logging for all tests in this module
    #[cfg(feature = "test-logging")]
    #[ctor::ctor]
    fn init_test_logging() {
        crate::test_utils::logging::init();
    }

    #[cfg(feature = "clangd-integration-tests")]
    #[tokio::test]
    async fn test_dynamic_component_discovery() {
        // Create test project and configure it
        let test_project = TestProject::new().await.unwrap();
        test_project.cmake_configure().await.unwrap();

        // Create empty workspace (no initial scan)
        let empty_workspace = ProjectWorkspace::new(
            test_project.project_root.clone(),
            vec![], // No components
            0,
        );

        // Verify workspace is actually empty
        assert_eq!(empty_workspace.component_count(), 0);
        assert!(
            empty_workspace
                .get_component_by_build_dir(&test_project.build_dir)
                .is_none()
        );

        // Create workspace session with empty workspace
        let clangd_path = crate::test_utils::get_test_clangd_path();
        let config = Arc::new(AppConfig::for_test(Path::new("."), clangd_path));
        let workspace_session = WorkspaceSession::new(empty_workspace, config).unwrap();

        // Request component session for build directory not in workspace
        // This should trigger dynamic discovery
        let component_session = workspace_session
            .get_component_session(test_project.build_dir.clone())
            .await;

        // Should succeed through dynamic discovery
        assert!(
            component_session.is_ok(),
            "Dynamic discovery should have succeeded"
        );
        let session = component_session.unwrap();
        assert_eq!(session.component().build_dir_path, test_project.build_dir);

        // Verify the component was added to the workspace
        {
            let workspace = workspace_session.get_workspace().lock().await;
            assert_eq!(workspace.component_count(), 1);
            assert!(
                workspace
                    .get_component_by_build_dir(&test_project.build_dir)
                    .is_some()
            );
        }

        // Second request should reuse cached session
        let second_session = workspace_session
            .get_component_session(test_project.build_dir.clone())
            .await
            .unwrap();

        // Verify it's the same session (Arc comparison)
        assert!(Arc::ptr_eq(&session, &second_session));
    }

    #[cfg(feature = "clangd-integration-tests")]
    #[tokio::test]
    async fn test_invalid_build_directory_fails() {
        // Create empty workspace
        let temp_dir = tempfile::tempdir().unwrap();
        let empty_workspace = ProjectWorkspace::new(temp_dir.path().to_path_buf(), vec![], 0);

        let clangd_path = crate::test_utils::get_test_clangd_path();
        let config = Arc::new(AppConfig::for_test(Path::new("."), clangd_path));
        let workspace_session = WorkspaceSession::new(empty_workspace, config).unwrap();

        // Request session for non-existent/invalid build directory
        let invalid_dir = temp_dir.path().join("not_a_build_dir");
        std::fs::create_dir_all(&invalid_dir).unwrap(); // Create directory but don't make it a build directory

        let result = workspace_session.get_component_session(invalid_dir).await;

        // Should fail as it's not a valid build directory
        assert!(result.is_err(), "Should fail for invalid build directory");
    }

    #[cfg(feature = "clangd-integration-tests")]
    #[tokio::test]
    async fn test_existing_component_not_rediscovered() {
        // Create test project and configure it
        let test_project = TestProject::new().await.unwrap();
        test_project.cmake_configure().await.unwrap();

        // Scan the project to create workspace with existing component
        let scanner = crate::project::ProjectScanner::with_default_providers();
        let workspace = scanner
            .scan_project(&test_project.project_root, 2, None)
            .unwrap();

        // Verify component is already in workspace
        assert_eq!(workspace.component_count(), 1);
        assert!(
            workspace
                .get_component_by_build_dir(&test_project.build_dir)
                .is_some()
        );

        // Create workspace session with pre-populated workspace
        let clangd_path = crate::test_utils::get_test_clangd_path();
        let config = Arc::new(AppConfig::for_test(Path::new("."), clangd_path));
        let workspace_session = WorkspaceSession::new(workspace, config).unwrap();

        // Request component session - should use existing component, not rediscover
        let component_session = workspace_session
            .get_component_session(test_project.build_dir.clone())
            .await;

        // Should succeed using existing component
        assert!(
            component_session.is_ok(),
            "Should succeed with existing component"
        );
        let session = component_session.unwrap();
        assert_eq!(session.component().build_dir_path, test_project.build_dir);

        // Verify workspace still has exactly one component (not duplicated)
        {
            let workspace = workspace_session.get_workspace().lock().await;
            assert_eq!(workspace.component_count(), 1);
        }
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use std::fs;

    fn write_db(build_dir: &Path, files: &[&str]) {
        let entries: Vec<serde_json::Value> = files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "file": f,
                    "directory": build_dir.to_string_lossy(),
                    "arguments": ["c++", "-c", f]
                })
            })
            .collect();
        fs::write(
            build_dir.join("compile_commands.json"),
            serde_json::to_string(&entries).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_synthesize_component_from_compile_commands() {
        let temp = tempfile::tempdir().unwrap();
        let build_dir = temp.path().join("out");
        fs::create_dir_all(&build_dir).unwrap();
        let src_dir = temp.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        // Create source files so canonicalization resolves them
        fs::write(src_dir.join("a.cpp"), "").unwrap();
        fs::write(src_dir.join("b.cpp"), "").unwrap();

        write_db(
            &build_dir,
            &[
                src_dir.join("a.cpp").to_str().unwrap(),
                src_dir.join("b.cpp").to_str().unwrap(),
            ],
        );

        let component = synthesize_component_from_compile_commands(&build_dir).unwrap();
        assert_eq!(component.build_dir_path, build_dir);
        assert_eq!(component.source_root_path, src_dir);
        assert_eq!(component.provider_type, "compile_commands");
        assert!(component.compilation_database_path.exists());
    }

    #[test]
    fn test_synthesize_component_missing_db() {
        let temp = tempfile::tempdir().unwrap();
        let build_dir = temp.path().join("out");
        fs::create_dir_all(&build_dir).unwrap();
        assert!(synthesize_component_from_compile_commands(&build_dir).is_none());
    }

    #[test]
    fn test_synthesize_component_invalid_db() {
        let temp = tempfile::tempdir().unwrap();
        let build_dir = temp.path().join("out");
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(build_dir.join("compile_commands.json"), "not json").unwrap();
        assert!(synthesize_component_from_compile_commands(&build_dir).is_none());
    }
}

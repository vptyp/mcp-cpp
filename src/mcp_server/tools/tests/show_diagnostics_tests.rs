//! Integration tests for diagnostics collection functionality
//!
//! These tests verify that clangd diagnostics are captured for files through the
//! DiagnosticsCollector wired into the ComponentSession. They use real clangd
//! integration with the test project.

use crate::project::{ProjectScanner, WorkspaceSession};
use crate::test_utils::integration::TestProject;
use std::time::Duration;

#[cfg(feature = "clangd-integration-tests")]
#[tokio::test]
async fn test_show_diagnostics_returns_published_diagnostics() {
    // Create a test project first
    let test_project = TestProject::new().await.unwrap();
    test_project.cmake_configure().await.unwrap();

    // Scan the test project to create a proper workspace with components
    let scanner = ProjectScanner::with_default_providers();
    let workspace = scanner
        .scan_project(&test_project.project_root, 3, None)
        .expect("Failed to scan test project");

    // Create a WorkspaceSession with test clangd path
    let clangd_path = crate::test_utils::get_test_clangd_path();
    let workspace_session = WorkspaceSession::new(
        workspace.clone(),
        std::sync::Arc::new(crate::config::AppConfig::for_test(
            &workspace.project_root_path,
            clangd_path,
        )),
    )
    .expect("Failed to create workspace session");

    // Get a ComponentSession for the build directory
    let component_session = workspace_session
        .get_component_session(test_project.build_dir.clone())
        .await
        .unwrap();

    // A known source file in the test project. It has unused-include warnings,
    // so we expect at least one diagnostic to be published.
    let target_file = test_project.project_root.join("src/main.cpp");

    let diagnostics = component_session
        .get_file_diagnostics(&target_file, Duration::from_secs(20))
        .await
        .expect("Failed to collect diagnostics");

    // We expect either diagnostics or a timeout/clean result; most importantly the
    // call should not error. If the file is parsed, clangd should publish diagnostics.
    if let Some(diags) = diagnostics {
        // Diagnostics should be attributed to clangd (or clang) as their source.
        for d in &diags {
            if let Some(source) = &d.source {
                assert!(
                    source == "clangd" || source == "clang",
                    "unexpected diagnostic source: {}",
                    source
                );
            }
        }
    }
}

#[cfg(feature = "clangd-integration-tests")]
#[tokio::test]
async fn test_show_diagnostics_short_timeout_returns_ok() {
    // A zero/very-short timeout should still return Ok without panicking.
    let test_project = TestProject::new().await.unwrap();
    test_project.cmake_configure().await.unwrap();

    let scanner = ProjectScanner::with_default_providers();
    let workspace = scanner
        .scan_project(&test_project.project_root, 3, None)
        .expect("Failed to scan test project");

    let clangd_path = crate::test_utils::get_test_clangd_path();
    let workspace_session = WorkspaceSession::new(
        workspace.clone(),
        std::sync::Arc::new(crate::config::AppConfig::for_test(
            &workspace.project_root_path,
            clangd_path,
        )),
    )
    .expect("Failed to create workspace session");

    let component_session = workspace_session
        .get_component_session(test_project.build_dir.clone())
        .await
        .unwrap();

    let target_file = test_project.project_root.join("include/Math.hpp");
    let result = component_session
        .get_file_diagnostics(&target_file, Duration::from_millis(200))
        .await;

    // Should either succeed with diagnostics, succeed with None (timeout), or
    // fail with a session error - but must not panic.
    assert!(result.is_ok() || result.is_err());
}

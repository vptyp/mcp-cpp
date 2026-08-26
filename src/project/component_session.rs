//! A clangd session for one build directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lsp_types::Diagnostic;
use tracing::{info, instrument, warn};

use crate::clangd::diagnostics::DiagnosticsCollector;
use crate::clangd::file_manager::ClangdFileManager;
use crate::clangd::session::ClangdSessionTrait;
use crate::clangd::{ClangdConfigBuilder, ClangdSession, ClangdSessionBuilder};
use crate::config::AppConfig;
use crate::lsp::traits::LspClientTrait;
use crate::project::{CompilationDatabase, ProjectComponent, ProjectError};

/// Owns the LSP connection and the client-side document state required by LSP.
pub struct ComponentSession {
    clangd_session: Arc<tokio::sync::Mutex<ClangdSession>>,
    file_manager: Arc<tokio::sync::Mutex<ClangdFileManager>>,
    component: ProjectComponent,
    diagnostics_collector: Arc<DiagnosticsCollector>,
    index_wait_timeout: Duration,
}

impl ComponentSession {
    #[instrument(name = "component_session_new", skip(component))]
    pub async fn new(
        component: ProjectComponent,
        app_config: &AppConfig,
        project_root: PathBuf,
    ) -> Result<Self, ProjectError> {
        let clangd = &app_config.clangd;
        let mut builder = ClangdConfigBuilder::new()
            .working_directory(project_root)
            .build_directory(component.build_dir_path.clone())
            .clangd_path(clangd.path.clone())
            .background_indexing(clangd.background_index)
            .pch_storage(clangd.pch_storage)
            .workspace_symbol_limit(clangd.workspace_symbol_limit)
            .initialization_timeout(clangd.initialization_timeout)
            .request_timeout(clangd.request_timeout);
        // Note: --log=verbose is intentionally NOT added. It generates a huge
        // stderr volume that adds parsing overhead and aligns the server's clangd
        // flags with VS Code's (which omits it). clangd's $/progress arrives via
        // the LSP client regardless of log verbosity.
        if let Some(threads) = clangd.index_threads {
            builder = builder.index_threads(threads);
        }
        let config = builder
            .add_args(clangd.extra_args.clone())
            .build()
            .map_err(|e| {
                ProjectError::SessionCreation(format!("Failed to build clangd configuration: {e}"))
            })?;
        let session = ClangdSessionBuilder::new()
            .with_config(config)
            .build()
            .await
            .map_err(|e| {
                ProjectError::SessionCreation(format!("Failed to create clangd session: {e}"))
            })?;
        let clangd_session = Arc::new(tokio::sync::Mutex::new(session));
        let diagnostics_collector = Arc::new(DiagnosticsCollector::new());
        {
            let session = clangd_session.lock().await;
            session
                .client()
                .register_notification_handler(diagnostics_collector.create_handler())
                .await;
        }

        // A fresh, short-lived CLI invocation otherwise has no document activity
        // to give clangd its first compilation context. Open one real translation
        // unit through the normal LSP path; clangd remains solely responsible for
        // all indexing and progress reporting. The first entry is streamed and
        // resolved against the database directory (its `file` is often relative,
        // e.g. Chromium's `../../chrome/...`); opening the raw relative path would
        // resolve against the process CWD, miss the file, and leave clangd with no
        // document to start its background index from.
        let mut file_manager = ClangdFileManager::new();
        if let Some(first_file) =
            CompilationDatabase::first_entry_resolved_path(&component.compilation_database_path)
        {
            let mut session = clangd_session.lock().await;
            if let Err(error) = file_manager
                .ensure_file_ready(&first_file, session.client_mut())
                .await
            {
                warn!(path = %first_file.display(), %error, "Failed to seed clangd with a translation unit");
            }
        }
        info!(build_dir = %component.build_dir_path.display(), "Created clangd session");
        Ok(Self {
            clangd_session,
            file_manager: Arc::new(tokio::sync::Mutex::new(file_manager)),
            component,
            diagnostics_collector,
            index_wait_timeout: app_config.clangd.index_wait_timeout,
        })
    }

    pub async fn ensure_file_ready(&self, path: &Path) -> Result<(), ProjectError> {
        let mut session = self.clangd_session.lock().await;
        self.file_manager
            .lock()
            .await
            .ensure_file_ready(path, session.client_mut())
            .await
            .map_err(|e| ProjectError::SessionCreation(format!("File management failed: {e}")))
    }

    pub async fn lsp_session(&self) -> tokio::sync::MutexGuard<'_, ClangdSession> {
        self.clangd_session.lock().await
    }

    /// Close a file in the LSP server.
    ///
    /// Used by the diagnostics flow to force a fresh `didOpen`: clangd only
    /// publishes `publishDiagnostics` when it (re)parses a file, and a `didOpen`
    /// for an already-open file is illegal in LSP. Closing first guarantees the
    /// subsequent `ensure_file_ready` sends a real `didOpen` and clangd reparses,
    /// so diagnostics are not lost when an earlier open happened while clangd was
    /// busy (e.g. mid index-load) and never published.
    pub async fn close_file(&self, path: &Path) -> Result<(), ProjectError> {
        let mut session = self.clangd_session.lock().await;
        let mut file_manager = self.file_manager.lock().await;

        file_manager
            .close_file(path, session.client_mut())
            .await
            .map_err(|e| ProjectError::SessionCreation(format!("File close failed: {e}")))
    }

    /// Collect fresh clangd diagnostics for a file.
    ///
    /// Diagnostics are pushed asynchronously by clangd only after a file is opened
    /// in the LSP session. This method:
    /// 1. Clears any cached diagnostics for the target file.
    /// 2. Ensures the file is open (opening or re-syncing it triggers a fresh publish).
    /// 3. Waits up to `timeout` for the published diagnostics.
    ///
    /// Returns the published diagnostics (possibly empty if the file is clean) or
    /// `Ok(None)` if clangd did not publish diagnostics within the timeout.
    pub async fn get_file_diagnostics(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<Option<Vec<Diagnostic>>, ProjectError> {
        let uri = crate::symbol::uri_from_pathbuf(path);
        self.diagnostics_collector.reset_for_uri(&uri).await;
        // Force a fresh `didOpen`: clangd only republishes diagnostics when it
        // (re)parses, and `ensure_file_ready` is a no-op for an already-open
        // unchanged file. If a prior open was lost (e.g. clangd was mid index-load
        // and never published), the file would otherwise stay "open" with no
        // diagnostics forever. Closing first makes the reopen a real didOpen.
        self.close_file(path).await.ok();

        // Open (or refresh) the file, which triggers clangd to publish diagnostics.
        self.ensure_file_ready(path).await?;
        Ok(self.diagnostics_collector.wait_for_uri(&uri, timeout).await)
    }

    pub fn component(&self) -> &ProjectComponent {
        &self.component
    }

    pub async fn index_status(&self) -> crate::clangd::progress::IndexStatus {
        let monitor = self
            .clangd_session
            .lock()
            .await
            .index_progress_monitor()
            .clone();
        monitor.status().await
    }

    pub async fn wait_for_index_status(
        &self,
        timeout: Duration,
    ) -> crate::clangd::progress::IndexStatus {
        let monitor = self
            .clangd_session
            .lock()
            .await
            .index_progress_monitor()
            .clone();
        monitor.wait_for_completion(timeout).await
    }

    pub fn index_wait_timeout(&self) -> Duration {
        self.index_wait_timeout
    }

    /// Legacy integration suites call this before exercising clangd. There is no
    /// client-side indexing phase anymore, so the call deliberately does nothing.
    #[cfg(all(test, feature = "clangd-integration-tests"))]
    pub async fn ensure_indexed(&self, _: Duration) -> Result<(), ProjectError> {
        Ok(())
    }
}

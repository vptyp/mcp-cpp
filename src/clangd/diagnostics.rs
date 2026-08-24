//! Diagnostics collection for clangd sessions
//!
//! LSP diagnostics are *pushed* by clangd via the `textDocument/publishDiagnostics`
//! notification rather than returned from a request. clangd only publishes
//! diagnostics for files that are currently open in the LSP session.
//!
//! This module provides a [`DiagnosticsCollector`] that registers a notification
//! handler on the LSP client, captures `publishDiagnostics` notifications into a
//! shared map, and lets callers wait for fresh diagnostics for a specific file.

use crate::lsp::protocol::JsonRpcNotification;
use lsp_types::notification::Notification;
use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{trace, warn};

/// A shared collection of the latest diagnostics published per file URI.
#[derive(Clone)]
pub struct DiagnosticsCollector {
    state: Arc<Mutex<DiagnosticsState>>,
}

#[derive(Default)]
struct DiagnosticsState {
    /// Latest diagnostics keyed by file URI (as a plain string).
    diagnostics: HashMap<String, Vec<Diagnostic>>,
}

impl DiagnosticsCollector {
    /// Create a new empty diagnostics collector.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(DiagnosticsState::default())),
        }
    }

    /// Create a notification handler that captures `publishDiagnostics` notifications.
    ///
    /// This returns a handler suitable for `LspClientTrait::register_notification_handler`.
    /// It satisfies the `'static` requirement by capturing only the shared state Arc and
    /// processes notifications on a background task so the LSP transport is never blocked.
    pub fn create_handler(&self) -> impl Fn(JsonRpcNotification) + Send + Sync + 'static {
        let state = Arc::clone(&self.state);
        move |notification| {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                Self::process_notification(notification, state).await;
            });
        }
    }

    /// Process a single LSP notification, storing diagnostics for matching files.
    async fn process_notification(
        notification: JsonRpcNotification,
        state: Arc<Mutex<DiagnosticsState>>,
    ) {
        if notification.method != lsp_types::notification::PublishDiagnostics::METHOD {
            return;
        }

        let params: Option<PublishDiagnosticsParams> = notification
            .params
            .and_then(|p| serde_json::from_value(p).ok());

        let Some(params) = params else {
            trace!("DiagnosticsCollector: could not parse publishDiagnostics params");
            return;
        };

        let uri = params.uri.to_string();
        let mut state = state.lock().await;
        // Always store the published set, even when empty (a clean file still
        // triggers an empty publish to clear prior diagnostics). Storing the empty
        // list lets callers distinguish "file is clean" from "no diagnostics yet".
        state.diagnostics.insert(uri, params.diagnostics);
    }

    /// Clear any cached diagnostics for the given file so the next published set
    /// for that file is treated as fresh. Call this right before (re)opening a file.
    pub async fn reset_for_uri(&self, uri: &Uri) {
        let mut state = self.state.lock().await;
        state.diagnostics.remove(&uri.to_string());
    }

    /// Retrieve the cached diagnostics for a file URI.
    pub async fn get_for_uri(&self, uri: &Uri) -> Option<Vec<Diagnostic>> {
        let state = self.state.lock().await;
        state.diagnostics.get(&uri.to_string()).cloned()
    }

    /// Wait for diagnostics to be published for the given URI, polling until the
    /// timeout elapses. Returns `Some(diagnostics)` if received, or `None` if no
    /// diagnostics were published before the timeout.
    pub async fn wait_for_uri(&self, uri: &Uri, timeout: Duration) -> Option<Vec<Diagnostic>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(diagnostics) = self.get_for_uri(uri).await {
                return Some(diagnostics);
            }
            if tokio::time::Instant::now() >= deadline {
                warn!(
                    "DiagnosticsCollector: timed out waiting for diagnostics for {}",
                    uri.to_string()
                );
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Default for DiagnosticsCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{DiagnosticSeverity, Position, Range};

    fn make_diagnostic(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 3,
                    character: 0,
                },
                end: Position {
                    line: 3,
                    character: 5,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("clangd".to_string()),
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn publish_notification(uri: &str, diagnostics: Vec<Diagnostic>) -> JsonRpcNotification {
        let params = PublishDiagnosticsParams {
            uri: uri.parse::<Uri>().unwrap(),
            version: None,
            diagnostics,
        };
        JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: lsp_types::notification::PublishDiagnostics::METHOD.to_string(),
            params: Some(serde_json::to_value(params).unwrap()),
        }
    }

    #[tokio::test]
    async fn test_default() {
        let collector = DiagnosticsCollector::default();
        assert!(collector.state.lock().await.diagnostics.is_empty());
    }

    #[tokio::test]
    async fn test_stores_diagnostics_for_publish_notification() {
        let collector = DiagnosticsCollector::new();
        let handler = collector.create_handler();

        let uri = "file:///tmp/test.cpp";
        handler(publish_notification(uri, vec![make_diagnostic("boom")]));

        // Give the background task time to process.
        let stored = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(d) = collector.get_for_uri(&uri.parse::<Uri>().unwrap()).await {
                    return d;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for stored diagnostics");

        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].message, "boom");
        assert_eq!(stored[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[tokio::test]
    async fn test_wait_for_uri_timeout_returns_none() {
        let collector = DiagnosticsCollector::new();
        let uri: Uri = "file:///tmp/none.cpp".parse().unwrap();
        let result = collector
            .wait_for_uri(&uri, Duration::from_millis(150))
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_reset_for_uri_clears_cache() {
        let collector = DiagnosticsCollector::new();
        let handler = collector.create_handler();
        let uri = "file:///tmp/test.cpp";
        let uri_typed: Uri = uri.parse().unwrap();

        handler(publish_notification(uri, vec![make_diagnostic("first")]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(collector.get_for_uri(&uri_typed).await.is_some());

        collector.reset_for_uri(&uri_typed).await;
        assert!(collector.get_for_uri(&uri_typed).await.is_none());
    }

    #[tokio::test]
    async fn test_ignores_non_diagnostic_notifications() {
        let collector = DiagnosticsCollector::new();
        let handler = collector.create_handler();

        handler(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "window/logMessage".to_string(),
            params: Some(serde_json::json!({ "type": 1, "message": "hi" })),
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let uri: Uri = "file:///tmp/other.cpp".parse().unwrap();
        assert!(collector.get_for_uri(&uri).await.is_none());
    }
}

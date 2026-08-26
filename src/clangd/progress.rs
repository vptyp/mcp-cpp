//! Direct view of clangd's standard background-index progress notifications.

use crate::lsp::protocol::JsonRpcNotification;
use lsp_types::notification::Notification;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};

/// The latest state reported by clangd. No filesystem or stderr inference is used.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexStatus {
    pub state: String,
    pub in_progress: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Default for IndexStatus {
    fn default() -> Self {
        Self {
            state: "NotStarted".to_string(),
            in_progress: false,
            percentage: None,
            message: None,
        }
    }
}

#[derive(Clone)]
pub struct IndexProgressMonitor {
    status: Arc<Mutex<IndexStatus>>,
    updates: watch::Sender<IndexStatus>,
}

impl IndexProgressMonitor {
    pub fn new() -> Self {
        let status = IndexStatus::default();
        let (updates, _) = watch::channel(status.clone());
        Self {
            status: Arc::new(Mutex::new(status)),
            updates,
        }
    }

    pub fn create_handler(&self) -> impl Fn(JsonRpcNotification) + Send + Sync + 'static {
        let monitor = self.clone();
        move |notification| {
            let monitor = monitor.clone();
            tokio::spawn(async move { monitor.handle(notification).await });
        }
    }

    pub async fn status(&self) -> IndexStatus {
        self.status.lock().await.clone()
    }

    pub async fn wait_for_completion(&self, timeout: std::time::Duration) -> IndexStatus {
        let mut updates = self.updates.subscribe();
        if updates.borrow().state == "Completed" {
            return updates.borrow().clone();
        }
        let _ = tokio::time::timeout(timeout, async {
            while updates.changed().await.is_ok() {
                if !updates.borrow().in_progress {
                    break;
                }
            }
        })
        .await;
        updates.borrow().clone()
    }

    async fn update(&self, status: IndexStatus) {
        *self.status.lock().await = status.clone();
        self.updates.send_replace(status);
    }

    async fn handle(&self, notification: JsonRpcNotification) {
        if notification.method != lsp_types::notification::Progress::METHOD {
            return;
        }
        let Some(params) = notification.params else {
            return;
        };
        if params.get("token").and_then(Value::as_str) != Some("backgroundIndexProgress") {
            return;
        }
        let Some(value) = params.get("value") else {
            return;
        };
        let percentage = value
            .get("percentage")
            .and_then(Value::as_u64)
            .map(|p| p as u8);
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") | Some("report") => {
                self.update(IndexStatus {
                    state: "InProgress".to_string(),
                    in_progress: true,
                    percentage,
                    message,
                })
                .await
            }
            Some("end") => {
                self.update(IndexStatus {
                    state: "Completed".to_string(),
                    in_progress: false,
                    percentage: Some(100),
                    message,
                })
                .await
            }
            _ => {}
        }
    }
}

impl Default for IndexProgressMonitor {
    fn default() -> Self {
        Self::new()
    }
}

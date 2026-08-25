//! Index status view for providing progress information to tools and users
//!
//! This module provides IndexStatusView which gives a high-level view of indexing
//! progress, including timing information and estimated completion times.

use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

/// High-level indexing status view for tools and end users
///
/// This struct provides comprehensive information about the current indexing
/// state, including progress, timing, and estimates. It's created on-demand
/// when tools request index status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatusView {
    /// Whether indexing is currently in progress
    pub in_progress: bool,

    /// Current progress percentage (0-100), None if not available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_percentage: Option<f32>,

    /// Number of files currently indexed
    pub indexed_files: usize,

    /// Total number of files to be indexed
    pub total_files: usize,

    /// When indexing started, None if not started or completed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<SystemTime>,

    /// Estimated time remaining for completion, None if cannot calculate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_time_remaining: Option<Duration>,

    /// Human-readable state description
    pub state: String,
}

impl IndexStatusView {
    /// Create a new IndexStatusView
    pub fn new(
        in_progress: bool,
        progress_percentage: Option<f32>,
        indexed_files: usize,
        total_files: usize,
        start_time: Option<SystemTime>,
        state: String,
    ) -> Self {
        let estimated_time_remaining = Self::calculate_eta(
            progress_percentage,
            indexed_files,
            total_files,
            start_time.as_ref(),
            in_progress,
        );

        Self {
            in_progress,
            progress_percentage,
            indexed_files,
            total_files,
            start_time,
            estimated_time_remaining,
            state,
        }
    }

    /// Calculate estimated time remaining based on current progress
    ///
    /// Prefers clangd's overall progress percentage (when available) because it
    /// tracks the real index-build progress. Falls back to a file-count-based
    /// estimate otherwise.
    ///
    /// Returns None if:
    /// - Not in progress
    /// - No start time available
    /// - No progress at all (can't extrapolate)
    fn calculate_eta(
        progress_percentage: Option<f32>,
        indexed_files: usize,
        total_files: usize,
        start_time: Option<&SystemTime>,
        in_progress: bool,
    ) -> Option<Duration> {
        // Can't calculate ETA if not in progress or no start time
        if !in_progress || start_time.is_none() {
            return None;
        }

        let start = start_time?;
        let elapsed = SystemTime::now().duration_since(*start).ok()?;

        // Avoid division by zero or negative elapsed time
        if elapsed.is_zero() {
            return None;
        }
        let elapsed_secs = elapsed.as_secs_f64();

        // Prefer the overall progress percentage (clangd's real progress). This is
        // robust even when the per-file indexed count lags far behind the actual
        // index build (which would otherwise produce absurd multi-day ETAs).
        if let Some(pct) = progress_percentage {
            let pct = pct.clamp(0.0, 100.0);
            if pct > 0.0 {
                let remaining_secs = elapsed_secs * (100.0 / pct as f64 - 1.0);
                return Some(Duration::from_secs_f64(remaining_secs.max(0.0)));
            }
        }

        // Fall back to the file-count-based estimate.
        if indexed_files == 0 {
            return None;
        }

        // Already completed
        if indexed_files >= total_files {
            return Some(Duration::ZERO);
        }

        // ETA formula: (total_files - indexed_files) * elapsed_time / indexed_files
        let remaining_files = total_files.saturating_sub(indexed_files);
        let eta_seconds = (remaining_files as f64 * elapsed_secs) / (indexed_files as f64);

        Some(Duration::from_secs_f64(eta_seconds))
    }

    /// Check if indexing is complete
    #[allow(dead_code)]
    pub fn is_complete(&self) -> bool {
        !self.in_progress && self.indexed_files >= self.total_files
    }

    /// Get completion percentage (0.0 to 1.0)
    #[allow(dead_code)]
    pub fn completion_ratio(&self) -> f32 {
        if self.total_files == 0 {
            1.0
        } else {
            self.indexed_files as f32 / self.total_files as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn test_index_status_view_creation() {
        let now = SystemTime::now();
        let view = IndexStatusView::new(
            true,
            Some(50.0),
            10,
            20,
            Some(now),
            "InProgress".to_string(),
        );

        assert!(view.in_progress);
        assert_eq!(view.progress_percentage, Some(50.0));
        assert_eq!(view.indexed_files, 10);
        assert_eq!(view.total_files, 20);
        assert_eq!(view.start_time, Some(now));
        assert_eq!(view.state, "InProgress");
        // ETA should be calculated since we have indexed files
        assert!(view.estimated_time_remaining.is_some());
    }

    #[test]
    fn test_eta_calculation_no_files_indexed() {
        let now = SystemTime::now();
        let view = IndexStatusView::new(
            true,
            Some(0.0),
            0, // No files indexed yet
            20,
            Some(now),
            "InProgress".to_string(),
        );

        // Should not have ETA because we can't divide by zero
        assert!(view.estimated_time_remaining.is_none());
    }

    #[test]
    fn test_eta_calculation_not_in_progress() {
        let now = SystemTime::now();
        let view = IndexStatusView::new(
            false, // Not in progress
            Some(100.0),
            20,
            20,
            Some(now),
            "Completed".to_string(),
        );

        // Should not have ETA because not in progress
        assert!(view.estimated_time_remaining.is_none());
    }

    #[test]
    fn test_eta_calculation_no_start_time() {
        let view = IndexStatusView::new(
            true,
            Some(50.0),
            10,
            20,
            None, // No start time
            "InProgress".to_string(),
        );

        // Should not have ETA because no start time
        assert!(view.estimated_time_remaining.is_none());
    }

    #[test]
    fn test_is_complete() {
        let view1 = IndexStatusView::new(false, Some(100.0), 20, 20, None, "Completed".to_string());
        assert!(view1.is_complete());

        let view2 = IndexStatusView::new(true, Some(50.0), 10, 20, None, "InProgress".to_string());
        assert!(!view2.is_complete());
    }

    #[test]
    fn test_completion_ratio() {
        let view1 = IndexStatusView::new(true, Some(50.0), 10, 20, None, "InProgress".to_string());
        assert_eq!(view1.completion_ratio(), 0.5);

        // Test edge case with 0 total files
        let view2 = IndexStatusView::new(false, None, 0, 0, None, "Completed".to_string());
        assert_eq!(view2.completion_ratio(), 1.0);
    }
}

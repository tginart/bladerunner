//! Purpose: This module holds the progress bar value of a task row.
//!
//! A task row is a background command or a subagent in the tasks pane. The
//! agent sets the value with `x.ai/task_progress` notifications. A row with
//! no value has no progress bar.

use std::collections::HashMap;

/// The total that marks a value as a percentage.
const PERCENT_TOTAL: u64 = 100;

/// Progress values of one agent view. The key is the task ID of a background
/// command or the subagent ID of a subagent.
///
/// Note: The map is not a field of the row structs. A value can arrive before
/// the notification that makes the row, and a map keeps that value.
pub type TaskProgressMap = HashMap<String, TaskProgressValue>;

/// One progress bar value: `completed` units of `total` units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskProgressValue {
    completed: u64,
    total: u64,
}

impl TaskProgressValue {
    /// Purpose: Make a value that a progress bar can show.
    /// Input: The completed units and the total units.
    /// Output: `None` when the total is zero. If not, a value with
    /// `completed` not larger than `total`.
    pub fn new(completed: u64, total: u64) -> Option<Self> {
        if total == 0 {
            return None;
        }
        Some(Self {
            completed: completed.min(total),
            total,
        })
    }

    /// The filled part of the bar, from 0.0 to 1.0.
    pub fn fraction(&self) -> f32 {
        self.completed as f32 / self.total as f32
    }

    /// The text next to the bar: `45%` for a percentage, `3/8` for a count.
    pub fn label(&self) -> String {
        if self.total == PERCENT_TOTAL {
            return format!("{}%", self.completed);
        }
        format!("{}/{}", self.completed, self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_total_has_no_value() {
        assert_eq!(TaskProgressValue::new(1, 0), None);
    }

    #[test]
    fn completed_is_clamped_to_total() {
        let value = TaskProgressValue::new(12, 8).unwrap();
        assert_eq!(value.label(), "8/8");
        assert_eq!(value.fraction(), 1.0);
    }

    #[test]
    fn percent_total_uses_a_percent_label() {
        assert_eq!(TaskProgressValue::new(45, 100).unwrap().label(), "45%");
        assert_eq!(TaskProgressValue::new(3, 8).unwrap().label(), "3/8");
    }
}

//! Port of the DTOs in `MediaBrowser.Model.Tasks`.
//!
//! The service interfaces (`ITaskManager`, `IScheduledTask`,
//! `IScheduledTaskWorker`, `IConfigurableScheduledTask`, `ITaskTrigger`),
//! `TaskCompletionEventArgs` and the `ScheduledTaskHelpers` helper are
//! server-side scheduler plumbing, not wire types, and are dropped from this
//! port.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::DayOfWeek;

/// The state of a scheduled task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum TaskState {
    /// The task is idle.
    #[default]
    Idle,
    /// The task is cancelling.
    Cancelling,
    /// The task is running.
    Running,
}

/// The completion status of a scheduled task run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum TaskCompletionStatus {
    /// The task completed.
    #[default]
    Completed,
    /// The task failed.
    Failed,
    /// The task was manually cancelled by the user.
    Cancelled,
    /// The task was aborted due to a system failure or shutdown.
    Aborted,
}

/// The type of a task trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum TaskTriggerInfoType {
    /// A daily trigger.
    #[default]
    DailyTrigger,
    /// A weekly trigger.
    WeeklyTrigger,
    /// An interval trigger.
    IntervalTrigger,
    /// A startup trigger.
    StartupTrigger,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    TaskTriggerInfoType, None, {
        DailyTrigger => ("DailyTrigger", 0),
        WeeklyTrigger => ("WeeklyTrigger", 1),
        IntervalTrigger => ("IntervalTrigger", 2),
        StartupTrigger => ("StartupTrigger", 3),
    }
}

/// Configuration for a task trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct TaskTriggerInfo {
    /// Gets or sets the type.
    #[serde(rename = "Type")]
    pub type_: TaskTriggerInfoType,

    /// Gets or sets the time of day in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub time_of_day_ticks: Option<i64>,

    /// Gets or sets the interval in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub interval_ticks: Option<i64>,

    /// Gets or sets the day of week.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub day_of_week: Option<DayOfWeek>,

    /// Gets or sets the maximum runtime in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub max_runtime_ticks: Option<i64>,
}

/// The result of a scheduled task execution.
///
/// A member a JSON document leaves out reads as C#'s default for it, as
/// `System.Text.Json` leaves it: `DateTime.MinValue` for a time, `Completed`
/// (enum 0) for the status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct TaskResult {
    /// Gets or sets the start time UTC.
    #[schema(value_type = String, format = "date-time")]
    #[serde(
        with = "crate::json::datetime",
        default = "crate::json::datetime::dotnet_min"
    )]
    pub start_time_utc: DateTime<Utc>,

    /// Gets or sets the end time UTC.
    #[schema(value_type = String, format = "date-time")]
    #[serde(
        with = "crate::json::datetime",
        default = "crate::json::datetime::dotnet_min"
    )]
    pub end_time_utc: DateTime<Utc>,

    /// Gets or sets the status.
    #[serde(default)]
    pub status: TaskCompletionStatus,

    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,

    /// Gets or sets the long error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_error_message: Option<String>,
}

/// A new C# `TaskResult`: every member at its type's default, so both times
/// are `DateTime.MinValue`, not chrono's Unix epoch.
impl Default for TaskResult {
    fn default() -> Self {
        Self {
            start_time_utc: crate::json::datetime::dotnet_min(),
            end_time_utc: crate::json::datetime::dotnet_min(),
            status: TaskCompletionStatus::default(),
            name: None,
            key: None,
            id: None,
            error_message: None,
            long_error_message: None,
        }
    }
}

/// Information about a scheduled task.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct TaskInfo {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the state of the task.
    pub state: TaskState,

    /// Gets or sets the current progress percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub current_progress_percentage: Option<f64>,

    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the last execution result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_execution_result: Option<TaskResult>,

    /// Gets or sets the triggers.
    pub triggers: Vec<TaskTriggerInfo>,

    /// Gets or sets the description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Gets or sets the category.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,

    /// Gets or sets a value indicating whether this instance is hidden.
    pub is_hidden: bool,

    /// Gets or sets the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{TaskCompletionStatus, TaskResult};

    /// `System.Text.Json` leaves a member the document lacks at its C#
    /// default, and `JsonDateTimeConverter` writes `DateTime.MinValue` with
    /// all seven tick digits.
    #[test]
    fn a_task_result_reads_missing_members_as_their_csharp_defaults() {
        let result: TaskResult = serde_json::from_str("{}").expect("parse");
        assert_eq!(result, TaskResult::default());
        assert_eq!(result.status, TaskCompletionStatus::Completed);
        assert_eq!(
            serde_json::to_value(&result).expect("serialize"),
            serde_json::json!({
                "StartTimeUtc": "0001-01-01T00:00:00.0000000Z",
                "EndTimeUtc": "0001-01-01T00:00:00.0000000Z",
                "Status": "Completed",
            })
        );
    }
}

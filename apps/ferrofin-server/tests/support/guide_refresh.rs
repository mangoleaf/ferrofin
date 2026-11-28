//! Waiting for the "Refresh Guide" run a Live TV configuration write queues.

use std::time::{Duration, Instant};

use ferrofin_model::tasks::{TaskCompletionStatus, TaskState};
use ferrofin_traits::tasks::TaskManager;

/// Waits until the guide is refreshed from a Live TV configuration write sent
/// after `since`: the "Refresh Guide" task is idle after a completed run that
/// started later. A run that started before `since` is not it, however it
/// ended: it is an earlier write's, which this write stopped.
///
/// # Panics
///
/// When that run did not complete, or none ended within 30 seconds.
pub async fn guide_refreshed_since(tasks: &dyn TaskManager, since: chrono::DateTime<chrono::Utc>) {
    let id = ferrofin_traits::tasks::task_id_for_key("RefreshGuide");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let task = tasks
            .get_task(&id)
            .await
            .expect("tasks")
            .expect("the Refresh Guide task");
        let run = task
            .last_execution_result
            .filter(|run| run.start_time_utc >= since);
        if task.state == TaskState::Idle
            && let Some(run) = run
        {
            assert_eq!(run.status, TaskCompletionStatus::Completed, "{run:?}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no guide refresh ended: the task is {:?}",
            task.state
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

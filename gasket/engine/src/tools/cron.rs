//! Cron tool for scheduling tasks

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, instrument};
use uuid::Uuid;

use super::{Tool, ToolContext, ToolError, ToolResult};
use crate::cron::CronService;

/// Cron tool for managing scheduled tasks
pub struct CronTool {
    service: std::sync::Arc<CronService>,
}

impl CronTool {
    /// Create a new cron tool
    pub fn new(service: std::sync::Arc<CronService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Schedule tasks to run at specific times. \
         Actions: 'add' creates a scheduled job with a name, cron expression (e.g., '0 9 * * *' for 9 AM daily), \
         and message; 'list' shows all jobs; 'remove' deletes a job by its ID; 'refresh' manually reloads all \
         cron files from disk; 'refresh_next_run' recalculates next execution times based on current time \
         for all jobs or a specific job by ID."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove", "refresh", "refresh_next_run"],
                    "description": "Action to perform: 'add' creates a job, 'list' shows all jobs, 'remove' deletes a job, 'refresh' reloads cron files from disk, 'refresh_next_run' recalculates next execution times based on current time"
                },
                "name": {
                    "type": "string",
                    "description": "Job name (required for add, e.g., 'Morning Report')"
                },
                "cron": {
                    "type": "string",
                    "description": "Cron expression (required for add). 7-field format: 'SEC MIN HOUR DAY MONTH WEEKDAY YEAR'. Examples: '0 0 9 * * * *' = 9 AM daily, '0 0 */6 * * * *' = every 6 hours, '0 */5 * * * * *' = every 5 minutes"
                },
                "message": {
                    "type": "string",
                    "description": "Message to send at scheduled time (required for add)"
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel (optional, defaults to current context channel). Examples: 'websocket', 'telegram', 'discord', 'cli'"
                },
                "to": {
                    "type": "string",
                    "description": "Target chat/user ID (optional, defaults to current context chat_id). For WebSocket, this is the user_id parameter"
                },
                "job_id": {
                    "type": "string",
                    "description": "Job ID (required for remove; optional for refresh_next_run to target a specific job; omit to refresh all jobs). Get this from the 'add' action response"
                }
            },
            "required": ["action"]
        })
    }

    #[instrument(name = "tool.cron", skip_all)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Args {
            action: String,
            name: Option<String>,
            cron: Option<String>,
            message: Option<String>,
            job_id: Option<String>,
            channel: Option<String>,
            to: Option<String>,
        }

        let args: Args =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        debug!("Cron tool invoked: action={}", args.action);

        match args.action.as_str() {
            "add" => {
                let name = args.name.ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "'name' is required for action 'add'. Example: 'Morning Report'"
                            .to_string(),
                    )
                })?;
                let cron = args.cron.ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "'cron' is required for action 'add'. Example: '0 9 * * *' for 9 AM daily"
                            .to_string(),
                    )
                })?;
                let message = args.message.ok_or_else(|| {
                    ToolError::InvalidArguments("'message' is required for action 'add'. This is the text that will be sent at the scheduled time".to_string())
                })?;

                let _: cron::Schedule = cron.parse().map_err(|e| {
                    ToolError::InvalidArguments(format!(
                        "Invalid cron expression '{}'. \
                         Requires 7-field format: 'SEC MIN HOUR DAY MONTH WEEKDAY YEAR'. \
                         Examples: '0 0 9 * * * *' (9 AM daily), '0 0 */6 * * * *' (every 6 hours). Error: {}",
                        cron, e
                    ))
                })?;

                let id = Uuid::new_v4().to_string();
                let channel = args.channel;
                let chat_id = args.to;

                let mut job = crate::cron::CronJob::new(&id, &name, &cron, &message);
                job.channel = channel;
                job.chat_id = chat_id;

                let channel_display = job.channel.as_deref().unwrap_or("cli").to_string();
                let chat_id_display = job.chat_id.as_deref().unwrap_or("").to_string();

                self.service.add_job(job).await.map_err(|e| {
                    ToolError::ExecutionError(format!(
                        "Failed to add cron job '{}': {}. Please check file system permissions in ~/.gasket/cron/",
                        name, e
                    ))
                })?;

                Ok(format!(
                    "✓ Scheduled job '{}'\n\nJob ID: {}\nCron: {}\nChannel: {}\nTo: {}\n\nUse this Job ID with action 'remove' to delete this job later.",
                    name, id, cron, channel_display, chat_id_display
                ))
            }
            "list" => {
                let jobs = self.service.list_jobs();
                if jobs.is_empty() {
                    return Ok("No scheduled jobs. Use action 'add' to create one.".to_string());
                }

                let mut result = format!("Scheduled jobs ({} total):\n", jobs.len());
                for job in jobs {
                    let next = job.next_run.map_or("N/A".to_string(), |t| {
                        t.format("%Y-%m-%d %H:%M UTC").to_string()
                    });
                    let status = if job.enabled { "✓" } else { "✗" };
                    result.push_str(&format!(
                        "\n{} {} (ID: {})\n  Message: {}\n  Cron: {}\n  Next run: {}\n",
                        status, job.name, job.id, job.message, job.cron, next
                    ));
                }
                Ok(result)
            }
            "remove" => {
                let job_id = args.job_id.ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "'job_id' is required for action 'remove'. \
                         Use action 'list' to see all job IDs, or get the ID from when you created the job with 'add'."
                            .to_string(),
                    )
                })?;

                let removed = self.service.remove_job(&job_id).await.map_err(|e| {
                    ToolError::ExecutionError(format!(
                        "Failed to remove cron job '{}': {}. Please check file system permissions.",
                        job_id, e
                    ))
                })?;

                if removed {
                    Ok(format!("✓ Removed job: {}", job_id))
                } else {
                    Ok(format!(
                        "Job not found: {}. Use action 'list' to see all available jobs.",
                        job_id
                    ))
                }
            }
            "refresh" => {
                let report = self.service.refresh_all_jobs().await.map_err(|e| {
                    ToolError::ExecutionError(format!("Failed to refresh cron jobs: {}", e))
                })?;

                Ok(format!(
                    "✓ Refreshed cron jobs\n\nLoaded: {}\nUpdated: {}\nRemoved: {}\nErrors: {}",
                    report.loaded, report.updated, report.removed, report.errors
                ))
            }
            "refresh_next_run" => {
                let results = self
                    .service
                    .refresh_next_run(args.job_id.as_deref())
                    .await
                    .map_err(|e| {
                        ToolError::ExecutionError(format!(
                            "Failed to refresh next run times: {}",
                            e
                        ))
                    })?;

                if results.is_empty() {
                    return Ok("No cron jobs to refresh.".to_string());
                }

                let mut output = format!("✓ Refreshed next run times ({} jobs)\n\n", results.len());
                for (id, name, next_run) in &results {
                    let next = next_run.map_or("N/A".to_string(), |t| {
                        t.format("%Y-%m-%d %H:%M UTC").to_string()
                    });
                    output.push_str(&format!("• {} ({}): {}\n", name, id, next));
                }
                Ok(output)
            }
            _ => Err(ToolError::InvalidArguments(format!(
                "Unknown action: '{}'. Valid actions are: 'add', 'list', 'remove', 'refresh', 'refresh_next_run'",
                args.action
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    /// Helper to create a CronService with a temp JSON state file for tests
    async fn create_test_cron_service(temp_dir: &std::path::Path) -> Arc<CronService> {
        let cron_path = temp_dir.join("state").join("cron.json");
        Arc::new(CronService::new(temp_dir.to_path_buf(), cron_path).await)
    }

    #[tokio::test]
    async fn test_cron_tool_add_missing_name() {
        let temp_dir = std::env::temp_dir().join(format!("gasket-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();

        let service = create_test_cron_service(&temp_dir).await;
        let tool = CronTool::new(service);

        let args = json!({
            "action": "add",
            "cron": "0 0 9 * * * *",
            "message": "Test"
        });

        let result = tool.execute(args, &ToolContext::default()).await;
        assert!(result.is_err());
        let err_msg = format!("{:?}", result);
        assert!(err_msg.contains("name"));

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_cron_tool_add_invalid_cron() {
        let temp_dir = std::env::temp_dir().join(format!("gasket-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();

        let service = create_test_cron_service(&temp_dir).await;
        let tool = CronTool::new(service);

        let args = json!({
            "action": "add",
            "name": "Test",
            "cron": "invalid format",
            "message": "Test"
        });

        let result = tool.execute(args, &ToolContext::default()).await;
        assert!(result.is_err());
        let err_msg = format!("{:?}", result);
        assert!(err_msg.contains("Invalid cron"));

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_cron_tool_add_and_list() {
        let temp_dir = std::env::temp_dir().join(format!("gasket-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();

        let service = create_test_cron_service(&temp_dir).await;
        let tool = CronTool::new(service.clone());

        // Add a job
        let args = json!({
            "action": "add",
            "name": "Test Job",
            "cron": "0 0 9 * * * *",
            "message": "Test message"
        });

        let result = tool.execute(args, &ToolContext::default()).await;
        assert!(result.is_ok(), "Add should succeed: {:?}", result);
        let response = result.unwrap();
        assert!(response.contains("Test Job"));
        assert!(response.contains("Job ID:"));

        // List jobs
        let list_args = json!({"action": "list"});
        let list_result = tool.execute(list_args, &ToolContext::default()).await;
        assert!(list_result.is_ok());
        let list_response = list_result.unwrap();
        assert!(list_response.contains("Test Job"));

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_cron_tool_read_after_write_consistency() {
        let temp_dir = std::env::temp_dir().join(format!("gasket-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();

        let service = create_test_cron_service(&temp_dir).await;
        let tool = CronTool::new(service.clone());

        // Add a job
        let args = json!({
            "action": "add",
            "name": "Consistency Test",
            "cron": "0 0 9 * * * *",
            "message": "Test"
        });

        let result = tool.execute(args, &ToolContext::default()).await;
        assert!(result.is_ok());

        // IMMEDIATELY list - should see the job without any sleep
        let list_args = json!({"action": "list"});
        let list_result = tool.execute(list_args, &ToolContext::default()).await;
        assert!(list_result.is_ok());
        let list_response = list_result.unwrap();
        assert!(
            list_response.contains("Consistency Test"),
            "Job should be immediately visible: {}",
            list_response
        );

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}

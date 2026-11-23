//! Trusted host operations reuse the command-tool process owner and readers.
use crate::tasks::{CommandRequest, CommandTaskManager};
use eliteforge_agent_core::{RunContext, ToolError};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
#[derive(Clone, Debug)]
pub struct CapturedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub env_remove: Vec<String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}
pub async fn capture_command(spec: CapturedCommand) -> Result<serde_json::Value, ToolError> {
    if !spec.cwd.is_absolute() || !spec.cwd.is_dir() {
        return Err(ToolError::Failed(
            "configured working directory is unavailable".into(),
        ));
    }
    if spec.timeout.is_zero()
        || spec.max_output_bytes == 0
        || spec.max_output_bytes > 32 * 1024 * 1024
    {
        return Err(ToolError::Failed("invalid command capture limits".into()));
    }
    let manager = CommandTaskManager::default();
    struct Stop(CommandTaskManager);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.stop_all();
        }
    }
    let _guard = Stop(manager.clone());
    let task = manager.start(
        CommandRequest {
            capture: None,
            session_id: "host-capture".into(),
            command: spec.program,
            args: spec.args,
            cwd: Some(spec.cwd),
            env: spec.env,
            env_remove: spec.env_remove,
            stdin: None,
            sudo: None,
            interactive: false,
            tty: false,
            rows: 24,
            cols: 80,
        },
        spec.timeout,
        spec.max_output_bytes,
    )?;
    task.wait_for_terminal(spec.timeout.saturating_add(Duration::from_secs(5)))
        .await;
    let output = serde_json::to_value(task.view(0, 0, spec.max_output_bytes))
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    manager.shutdown().await.map_err(ToolError::Failed)?;
    Ok(output)
}
/// Safe for synchronous adapters called inside or outside a Tokio runtime.
/// Only waiting is isolated; process control stays in the command-task manager.
pub fn capture_command_blocking(spec: CapturedCommand) -> Result<serde_json::Value, ToolError> {
    host_operation_blocking(capture_command(spec))
}
/// Compatibility bridge for trusted synchronous host adapters. Asynchronous
/// runtime adapters should await the same operation directly.
pub fn host_operation_blocking<F, T>(operation: F) -> Result<T, ToolError>
where
    F: std::future::Future<Output = Result<T, ToolError>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ToolError::Failed(error.to_string()))?
            .block_on(operation)
    })
    .join()
    .map_err(|_| ToolError::Failed("host operation thread failed".into()))?
}

/// Capture a trusted host command inside an existing run cancellation/deadline.
/// Dropping the selected capture future uses the existing native process owner's
/// stop guard; no independent blocking waiter survives the run.
pub async fn capture_command_in_run(
    spec: CapturedCommand,
    run: &RunContext,
) -> Result<serde_json::Value, ToolError> {
    run.check_cancelled()
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    capture_command_with_cancel(spec, run.cancelled(), run.deadline).await
}
/// Integrate the native capture owner with a host's existing cooperative control.
pub async fn capture_command_with_cancel(
    spec: CapturedCommand,
    cancelled: impl std::future::Future<Output = ()> + Send,
    deadline: tokio::time::Instant,
) -> Result<serde_json::Value, ToolError> {
    tokio::select! {
        biased;
        _ = cancelled => Err(ToolError::Indeterminate("command capture cancelled; process stop requested".into())),
        _ = tokio::time::sleep_until(deadline) => Err(ToolError::Indeterminate("command capture deadline exceeded; process stop requested".into())),
        output = capture_command(spec) => output,
    }
}

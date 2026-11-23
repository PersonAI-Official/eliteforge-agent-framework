//! Host-owned command output files. Temporary files must be removed on drop;
//! published references follow the host session's authorization and cleanup.
use async_trait::async_trait;
use eliteforge_agent_core::{RunContext, ToolError};
use serde_json::Value;
use std::{io::Write, sync::Arc};

/// A host writer publishes its bytes into session storage when capture finishes.
/// Empty streams need no artifact and are finalized by dropping the writer.
pub trait CommandOutputWriter: Write + Send {
    fn publish(&mut self) -> Result<Value, String>;
}

pub struct CommandOutputFile {
    pub writer: Box<dyn CommandOutputWriter>,
    pub max_bytes: u64,
}

#[async_trait]
pub trait CommandOutputCaptureProvider: Send + Sync {
    async fn create(&self, stream: &str, run: &RunContext) -> Result<CommandOutputFile, ToolError>;
}

#[derive(Clone)]
pub struct CommandOutputCaptureService(pub Arc<dyn CommandOutputCaptureProvider>);

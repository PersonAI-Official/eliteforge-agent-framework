//! Capability-pack boundary for file coordination. Storage and authorization stay outside it.
use async_trait::async_trait;
use eliteforge_agent_core::{RunContext, ToolError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

mod local;
pub use local::LocalFileCoordination;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResource {
    pub storage_namespace: String,
    pub workspace_id: String,
    pub path: String,
    /// Local alias only. Remote backends use the stable identity above.
    pub physical_alias: String,
}
impl PartialEq for FileResource {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for FileResource {}
impl PartialOrd for FileResource {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for FileResource {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.storage_namespace, &self.workspace_id, &self.path).cmp(&(
            &other.storage_namespace,
            &other.workspace_id,
            &other.path,
        ))
    }
}
impl FileResource {
    pub fn key(&self) -> String {
        serde_json::to_string(&(&self.storage_namespace, &self.workspace_id, &self.path))
            .expect("resource key")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileSubject {
    pub security_domain: String,
    pub session_id: String,
    pub task_instance_id: String,
}
/// Trusted host identity. Children must receive a distinct task instance.
#[derive(Debug, Clone)]
pub struct FileCoordinationIdentity(pub FileSubject);
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileFragment {
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileObservation {
    pub resource: FileResource,
    pub subject: FileSubject,
    pub authorization_revision: String,
    pub fingerprint: String,
    pub total_lines: usize,
    pub complete: bool,
    pub fragments: Vec<FileFragment>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileEditStatus {
    NotApplied,
    Applied,
    RolledBack,
    Indeterminate,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEditOutcome {
    pub subject: FileSubject,
    pub status: FileEditStatus,
    pub resources: Vec<FileResource>,
    pub observations: Vec<FileObservation>,
}
#[derive(Debug, Clone, Default)]
pub struct FileInvalidation {
    pub subject: Option<FileSubject>,
    pub session_id: Option<String>,
    pub authorization_revision: Option<String>,
    pub resources: Vec<FileResource>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum FileCoordinationScope {
    Process,
    DistributedLease,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCoordinationCapabilities {
    pub scope: FileCoordinationScope,
    pub renewable_leases: bool,
    pub storage_enforced_fencing: bool,
}
/// An opaque owned lease. Drop releases local ownership; remote adapters must
/// also arrange bounded cleanup/expiry. No filesystem or Redis handles escape.
#[async_trait]
pub trait FileCoordinationLease: Send + Sync {
    fn resources(&self) -> &[FileResource];
    fn ownership_token(&self) -> &str;
    async fn validate(&self) -> Result<(), ToolError>;
    async fn renew(&self) -> Result<(), ToolError>;
    /// Completes when ownership is lost. A non-expiring local lease stays pending.
    async fn lost(&self);
    async fn release(&self) -> Result<(), ToolError>;
}
#[eliteforge_agent_core::extension_point(single)]
#[async_trait]
pub trait FileCoordinationBackend: Send + Sync {
    fn capabilities(&self) -> FileCoordinationCapabilities;
    async fn acquire_many(
        &self,
        resources: Vec<FileResource>,
        run: &RunContext,
    ) -> Result<Arc<dyn FileCoordinationLease>, ToolError>;
    async fn observations(
        &self,
        subject: &FileSubject,
        revision: &str,
        resources: &[FileResource],
    ) -> Result<Vec<FileObservation>, ToolError>;
    async fn record_read(
        &self,
        lease: &dyn FileCoordinationLease,
        observation: FileObservation,
    ) -> Result<(), ToolError>;
    async fn publish(
        &self,
        lease: &dyn FileCoordinationLease,
        outcome: FileEditOutcome,
    ) -> Result<(), ToolError>;
    async fn invalidate(&self, invalidation: FileInvalidation) -> Result<(), ToolError>;
}
/// Install through any capability pack. The single extension rejects duplicates.
pub struct FileCoordinationPlugin(pub Arc<dyn FileCoordinationBackend>);
#[async_trait]
impl eliteforge_agent_core::Plugin for FileCoordinationPlugin {
    fn id(&self) -> &str {
        "framework.filesystem.coordination"
    }
    async fn install(
        &self,
        context: &mut eliteforge_agent_core::PluginContext<'_>,
    ) -> Result<(), eliteforge_agent_core::PluginError> {
        context.contribute_extension(self.0.clone())
    }
}
/// The same fallback is shared across providers AND Framework instances.
pub fn default_file_coordination() -> Arc<dyn FileCoordinationBackend> {
    static DEFAULT: std::sync::OnceLock<Arc<LocalFileCoordination>> = std::sync::OnceLock::new();
    DEFAULT
        .get_or_init(|| Arc::new(LocalFileCoordination::default()))
        .clone()
}
/// Resolved after every plugin has installed. Never falls back after selection.
#[derive(Clone, Default)]
pub(crate) struct CoordinationSelection(
    pub Arc<std::sync::OnceLock<Arc<dyn FileCoordinationBackend>>>,
);
impl CoordinationSelection {
    pub fn ready(&self, context: &eliteforge_agent_core::PluginReadyContext<'_>) {
        let backend = context
            .extension::<dyn FileCoordinationBackend>()
            .unwrap_or_else(default_file_coordination);
        let _ = self.0.set(backend);
    }
    pub fn backend(&self) -> Result<Arc<dyn FileCoordinationBackend>, ToolError> {
        self.0
            .get()
            .cloned()
            .ok_or_else(|| ToolError::Failed("file coordination provider is not ready".into()))
    }
}

impl std::fmt::Debug for CoordinationSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinationSelection")
            .field("ready", &self.0.get().is_some())
            .finish()
    }
}

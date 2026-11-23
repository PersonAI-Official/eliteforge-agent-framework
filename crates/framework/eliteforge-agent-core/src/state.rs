//! Durable execution contracts. Checkpoints are distinct from model projections.
use crate::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExecutionStatus {
    Running,
    Waiting,
    Completed,
    Cancelled,
    Failed,
}
impl ExecutionStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

/// Persist intent before invoking any tool. Started without a durable result is
/// indeterminate after losing the executor; it is never an automatic retry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToolExecutionState {
    Planned,
    Approved,
    WaitingApproval { interaction_id: RunId },
    Started,
    Finished(ToolOutput),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolExecution {
    pub invocation_id: RunId,
    pub tool_id: String,
    pub descriptor: ToolDescriptor,
    pub call: ToolCall,
    pub state: ToolExecutionState,
    /// The model addressed this call through the `tool_execute` gateway
    /// envelope; admission rewrote `call` to the inner tool's call. Old
    /// checkpoints deserialize to false.
    #[serde(default)]
    pub via_gateway: bool,
    /// Provider call whose result contains this independently admitted item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_item: Option<ToolBatchItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation: Option<ToolPreparation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum InteractionKind {
    Input,
    Approval { invocation_id: RunId },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionRequest {
    pub id: RunId,
    pub kind: InteractionKind,
    pub prompt: Content,
    pub response_schema: Value,
    /// UI data supplied by a tool; independent of any transport protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<Value>,
    pub expires_at_unix_ms: Option<u64>,
}

impl InteractionRequest {
    pub fn input(prompt: impl Into<Content>, response_schema: Value) -> Result<Self, AgentError> {
        compile_response_schema(&response_schema)?;
        Ok(Self {
            id: RunId::new_v4(),
            kind: InteractionKind::Input,
            prompt: prompt.into(),
            response_schema,
            presentation: None,
            expires_at_unix_ms: None,
        })
    }
    pub fn approval(invocation_id: RunId, prompt: impl Into<Content>) -> Self {
        Self {
            id: RunId::new_v4(),
            kind: InteractionKind::Approval { invocation_id },
            prompt: prompt.into(),
            response_schema: serde_json::json!({"type":"boolean"}),
            presentation: None,
            expires_at_unix_ms: None,
        }
    }
    pub fn validate_response(
        &self,
        response: &InteractionResponse,
        now_unix_ms: u64,
    ) -> Result<(), AgentError> {
        if response.interaction_id != self.id
            || self
                .expires_at_unix_ms
                .is_some_and(|expiry| now_unix_ms >= expiry)
        {
            return Err(AgentError::InvalidConfiguration(
                "stale, expired or mismatched interaction response".into(),
            ));
        }
        let validator = compile_response_schema(&self.response_schema)?;
        let errors: Vec<_> = validator
            .iter_errors(&response.value)
            .take(16)
            .map(|error| format!("{}: {}", error.instance_path(), error.schema_path()))
            .collect();
        if !errors.is_empty() {
            return Err(AgentError::InvalidConfiguration(format!(
                "interaction response does not match schema: {}",
                errors.join("; ")
            )));
        }
        if matches!(self.kind, InteractionKind::Approval { .. }) && !response.value.is_boolean() {
            return Err(AgentError::InvalidConfiguration(
                "approval requires an explicit boolean decision".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionResponse {
    pub interaction_id: RunId,
    pub value: Value,
}

#[derive(Debug, Clone, Default)]
pub struct ResumeRequest {
    pub run_id: RunId,
    pub response: Option<InteractionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolPolicyDecision {
    Allow,
    Deny(String),
    RequireApproval(Content),
}

/// Admission of an original provider binding into the run's tool catalog.
/// Hiding only narrows initial exposure; rejection never discards lease resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolBindingAdmission {
    Allow,
    Hidden,
    Exclude,
}

#[crate::extension_point(adapter = crate::ToolPolicyPlugin, contribute)]
#[async_trait]
pub trait ToolPolicy: Send + Sync {
    /// Apply host authorization before duplicate catalog detection. This does
    /// not execute a tool or request approval; evaluate remains authoritative
    /// for every invocation, including changes to live permissions.
    async fn admit_binding(
        &self,
        _binding: &ToolBinding,
        _run: &RunContext,
    ) -> Result<ToolBindingAdmission, AgentError> {
        Ok(ToolBindingAdmission::Allow)
    }

    /// Observe the final validated catalog after every admission policy has
    /// narrowed it. Hosts may project descriptor metadata; original executors
    /// and lease ownership remain unchanged.
    async fn catalog_ready(
        &self,
        _bindings: &[ToolBinding],
        _run: &RunContext,
    ) -> Result<(), AgentError> {
        Ok(())
    }

    async fn evaluate(
        &self,
        invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError>;
}

/// One atomic durable state transition. `state` is owned and versioned by the
/// execution strategy. The transcript and event log commit in the same write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(default)]
    pub host_metadata: RunHostMetadata,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capability_versions: Vec<CapabilityVersionBinding>,
    pub schema_version: u32,
    pub run_id: RunId,
    pub session_id: String,
    #[serde(default = "default_execution_scope")]
    pub execution_scope: String,
    pub revision: u64,
    pub loop_id: String,
    pub loop_version: u32,
    pub status: ExecutionStatus,
    pub state: Value,
    pub transcript: Vec<Message>,
    /// Ownership of this run's suffix in the persistent conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_span: Option<ConversationSpan>,
    pub events: Vec<EventEnvelope>,
    /// Public records for this run only; private transcript is never a history API.
    #[serde(default)]
    pub history: crate::HistoryRecords,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationSpan {
    pub key: crate::ConversationKey,
    pub generation: String,
    pub start: usize,
}

fn default_execution_scope() -> String {
    "main".into()
}

impl Checkpoint {
    pub fn with_host_metadata(mut self, request: &RunRequest) -> Self {
        self.host_metadata = request.host_metadata();
        self
    }
    pub fn with_execution_scope(mut self, scope: impl Into<String>) -> Self {
        self.execution_scope = scope.into();
        self
    }
    pub fn new(
        run_id: RunId,
        session_id: impl Into<String>,
        loop_id: impl Into<String>,
        loop_version: u32,
        state: Value,
    ) -> Self {
        Self {
            host_metadata: RunHostMetadata::default(),
            capability_versions: Vec::new(),
            schema_version: 1,
            run_id,
            session_id: session_id.into(),
            execution_scope: default_execution_scope(),
            revision: 0,
            loop_id: loop_id.into(),
            loop_version,
            status: ExecutionStatus::Running,
            state,
            transcript: Vec::new(),
            conversation_span: None,
            events: Vec::new(),
            history: Default::default(),
        }
    }
    pub fn validate(&self) -> Result<(), StateError> {
        self.history.validate(self.run_id)?;
        if let Some(span) = &self.conversation_span {
            if span.key.session_id != self.session_id
                || span.generation.is_empty()
                || span.start > self.transcript.len()
            {
                return Err(StateError::Invalid("invalid conversation span".into()));
            }
        }
        if self.schema_version != 1 || self.loop_version == 0 {
            return Err(StateError::UnsupportedVersion);
        }
        if self.run_id.is_nil()
            || self.session_id.trim().is_empty()
            || self.execution_scope.trim().is_empty()
            || self.loop_id.trim().is_empty()
        {
            return Err(StateError::Invalid(
                "checkpoint identity must be nonempty".into(),
            ));
        }
        validate_messages(&self.transcript).map_err(|_| {
            StateError::Invalid("checkpoint transcript contains incomplete tool exchanges".into())
        })?;
        let mut previous = 0;
        for event in &self.events {
            if event.schema_version != 1
                || event.scope != crate::EventScope::Journal
                || event.run_id != self.run_id
                || event.session_id != self.session_id
                || event.sequence <= previous
            {
                return Err(StateError::Invalid(
                    "checkpoint events have invalid identity or order".into(),
                ));
            }
            previous = event.sequence;
        }
        Ok(())
    }
    pub fn validate_successor(&self, next: &Self) -> Result<(), StateError> {
        next.validate()?;
        self.history.validate_successor(&next.history)?;
        if self.run_id != next.run_id
            || self.session_id != next.session_id
            || self.execution_scope != next.execution_scope
            || self.conversation_span != next.conversation_span
            || self.host_metadata != next.host_metadata
            || self.loop_id != next.loop_id
            || self.loop_version != next.loop_version
        {
            return Err(StateError::Invalid(
                "checkpoint identity cannot change".into(),
            ));
        }
        if self.status.is_terminal() {
            return Err(StateError::Conflict);
        }
        if !next.transcript.starts_with(&self.transcript) {
            return Err(StateError::Invalid(
                "source transcript is append-only".into(),
            ));
        }
        if next.events.len() < self.events.len()
            || next
                .events
                .iter()
                .zip(&self.events)
                .any(|(a, b)| serde_json::to_value(a).ok() != serde_json::to_value(b).ok())
        {
            return Err(StateError::Invalid(
                "durable event log is append-only".into(),
            ));
        }
        Ok(())
    }
}

/// Stable fencing identity for one ownership period, not an in-process mutex.
/// `run_id`, `owner`, and `generation` stay unchanged across successful renewals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionLease {
    pub run_id: RunId,
    pub owner: RunId,
    pub generation: u64,
    /// Informational expiry snapshot at claim/renew time. Stores must validate
    /// against their authoritative expiry/clock, not this possibly stale value.
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum StateError {
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error("execution state not found")]
    NotFound,
    #[error("execution state revision or predecessor conflict")]
    Conflict,
    #[error("session or run already has an executor")]
    Busy,
    #[error("execution lease lost or expired")]
    LeaseLost,
    #[error("unsupported execution state version")]
    UnsupportedVersion,
    #[error("invalid execution state: {0}")]
    Invalid(String),
    #[error("execution storage error: {0}")]
    Backend(String),
}

#[crate::extension_point(single, adapter = crate::StateStorePlugin)]
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Atomically replace the session/scope head iff it is `previous_run` and terminal.
    /// A new checkpoint must retain that head's transcript as an exact prefix.
    async fn create(
        &self,
        checkpoint: Checkpoint,
        previous_run: Option<RunId>,
    ) -> Result<Checkpoint, StateError>;
    async fn load(&self, run_id: RunId) -> Result<Checkpoint, StateError>;
    async fn latest(&self, session_id: &str) -> Result<Option<Checkpoint>, StateError>;
    /// Latest checkpoint in one execution scope; scopes are independent heads.
    async fn latest_scope(
        &self,
        session_id: &str,
        scope: &str,
    ) -> Result<Option<Checkpoint>, StateError>;
    /// Acquire exclusive ownership with a newer fencing generation. Reclaiming
    /// an expired/released lease must fence its former owner, even if reused.
    async fn claim(
        &self,
        run_id: RunId,
        owner: RunId,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError>;
    /// Extend the live ownership period without changing its fencing identity.
    /// Earlier expiry snapshots remain valid for renew/commit/release. Check
    /// the store's authoritative expiry atomically; never revive a lost lease.
    async fn renew(
        &self,
        lease: &ExecutionLease,
        ttl: Duration,
    ) -> Result<ExecutionLease, StateError>;
    /// Validate fencing and revision and commit the whole checkpoint atomically.
    /// Return the committed revision. Never mutate a newer writer's state.
    async fn commit(
        &self,
        lease: &ExecutionLease,
        expected_revision: u64,
        next: Checkpoint,
    ) -> Result<Checkpoint, StateError>;
    /// Release only this fencing identity. Retrying after an uncertain outcome
    /// must not affect a newer owner; already released/lost leases may return
    /// `LeaseLost`, which the runtime treats as successful cleanup.
    async fn release(&self, lease: &ExecutionLease) -> Result<(), StateError>;
}

pub struct StateStoreService(pub Arc<dyn StateStore>);
pub struct StateStorePlugin(pub Arc<dyn StateStore>);
impl StateStorePlugin {
    pub fn new(store: impl StateStore + 'static) -> Self {
        Self(Arc::new(store))
    }

    pub fn from_arc(store: Arc<dyn StateStore>) -> Self {
        Self(store)
    }
}
#[async_trait]
impl Plugin for StateStorePlugin {
    fn id(&self) -> &str {
        "framework.state-store"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(StateStoreService(self.0.clone()))?;
        context.contribute_extension(self.0.clone())
    }
}

pub fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

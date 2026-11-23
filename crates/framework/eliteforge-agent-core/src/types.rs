use std::{
    any::{Any, TypeId},
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub type RunId = uuid::Uuid;

/// Opaque host data. Intentionally neither serializable nor printed by Debug.
#[derive(Clone, Default)]
pub struct Extensions(HashMap<TypeId, Arc<dyn Any + Send + Sync>>);

impl Extensions {
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.0.insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn remove<T: Send + Sync + 'static>(&mut self) {
        self.0.remove(&TypeId::of::<T>());
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.0.get(&TypeId::of::<T>())?.clone().downcast().ok()
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("count", &self.0.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Framework invocation identity, distinct from the provider's protocol ID.
    /// Assigned per returned model call and retained through execution/recovery.
    /// Old transcripts without this field deserialize to nil; durable workflows
    /// restore it from their persisted ToolExecution identity before use.
    #[serde(default)]
    pub invocation_id: RunId,
    pub id: String,
    pub call_name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, call_name: impl Into<String>, arguments: Value) -> Self {
        Self {
            invocation_id: RunId::new_v4(),
            id: id.into(),
            call_name: call_name.into(),
            arguments,
        }
    }
}

/// Display identity supplied by the trusted host, independent of provider role.
/// `kind` is host-owned (for example user, agent, or system).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MessageSender {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

/// Public attribution. Never use model-generated values as authorization data.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MessageProvenance {
    pub sender: MessageSender,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<Value>,
}

/// Serializable host metadata carried through checkpoints and delegation.
/// The framework stores it without interpreting ownership or access rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunHostMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_provenance: Option<MessageProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_provenance: Option<MessageProvenance>,
}

/// Shared so observers created before asynchronous checkpoint recovery see the
/// same restored metadata as tools and lifecycle hooks.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct RunHostMetadataService(pub Arc<std::sync::RwLock<RunHostMetadata>>);

/// Trusted instruction origin retained until a model adapter chooses its wire role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InstructionOrigin {
    Agent,
    Plugin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    #[serde(default)]
    pub annotations: Vec<crate::MessageAnnotation>,
    pub role: Role,
    /// Provider-supplied assistant message phase; absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Assistant messages produced by the same model response share this ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_origin: Option<InstructionOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MessageProvenance>,
    pub content: crate::Content,
    /// Structured assistant output, retained alongside the original content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResultMetadata>,
    /// Adapter-owned continuation data, scoped to the originating model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context: Option<crate::ModelMessageContext>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<crate::Content>) -> Self {
        Self {
            role,
            phase: None,
            output_group_id: None,
            instruction_origin: None,
            annotations: Vec::new(),
            provenance: None,
            content: content.into(),
            structured: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_result: None,
            model_context: None,
        }
    }

    pub fn with_annotations(mut self, annotations: Vec<crate::MessageAnnotation>) -> Self {
        self.annotations = annotations;
        self
    }

    pub fn with_instruction_origin(mut self, origin: InstructionOrigin) -> Self {
        self.instruction_origin = Some(origin);
        self
    }

    pub fn with_provenance(mut self, provenance: Option<MessageProvenance>) -> Self {
        self.provenance = provenance;
        self
    }

    pub fn user(content: impl Into<crate::Content>) -> Self {
        Self::new(Role::User, content)
    }
    pub fn system(content: impl Into<crate::Content>) -> Self {
        Self::new(Role::System, content)
    }
    pub fn assistant(content: impl Into<crate::Content>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<crate::Content>) -> Self {
        Self {
            role: Role::Tool,
            phase: None,
            output_group_id: None,
            instruction_origin: None,
            annotations: Vec::new(),
            provenance: None,
            content: content.into(),
            structured: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            tool_result: None,
            model_context: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolResultMetadata {
    pub is_error: bool,
    pub structured: Option<Value>,
    /// Complete original ToolOutput, when large result data was externalized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_reference: Option<Box<crate::ResourceReference>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    #[doc(hidden)]
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

/// Provider-declared side effects, used for tool selection rather than isolation.
/// Unknown tools must never be implicitly treated as read-only.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolOperationKind {
    #[default]
    Unknown,
    Query,
    Command,
}

/// Selects the deadline for the tool body after admission. Tools that wait for
/// another Agent use the enclosing run deadline instead of the ordinary tool
/// timeout; user cancellation still interrupts the wait.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolTimeoutScope {
    #[default]
    Tool,
    Run,
}

/// Model-facing input contract in gateway mode. Atomic provider implementations
/// remain internal; each batch item follows the normal invocation pipeline.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolInputMode {
    #[default]
    Batch,
    /// Stateful interactions or file edits that require one controlled operation.
    Single,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDescriptor {
    /// Stable host/catalog identity; never inferred from the model call name.
    pub id: String,
    /// Model-visible function name. Must be unique within a run.
    pub call_name: String,
    pub description: String,
    pub input_schema: Value,
    /// Optional model-facing projection of `input_schema`. Executors and
    /// validation keep using `input_schema`, so a host can hide a legacy field
    /// from new model calls while still detecting and rejecting it explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_input_schema: Option<Value>,
    /// Human-facing plugin resources; excluded from model function projections.
    #[serde(default)]
    pub display: Option<crate::ToolDisplayMetadata>,
    /// A declaration, not a guarantee that the executor cannot modify data.
    #[serde(default)]
    pub operation_kind: ToolOperationKind,
    #[serde(default)]
    pub timeout_scope: ToolTimeoutScope,
    #[serde(default)]
    pub input_mode: ToolInputMode,
}

impl ToolDescriptor {
    pub fn new(
        id: impl Into<String>,
        call_name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            id: id.into(),
            call_name: call_name.into(),
            description: description.into(),
            input_schema,
            model_input_schema: None,
            display: None,
            operation_kind: ToolOperationKind::Unknown,
            timeout_scope: ToolTimeoutScope::Tool,
            input_mode: ToolInputMode::Batch,
        }
    }

    /// Use a narrower schema only for model-facing tool lists and `tool_schema`.
    /// Admission, validation and the executor continue to use `input_schema`.
    pub fn with_model_input_schema(mut self, schema: Value) -> Self {
        self.model_input_schema = Some(schema);
        self
    }

    pub fn with_single_input(mut self) -> Self {
        self.input_mode = ToolInputMode::Single;
        self
    }

    pub fn with_operation_kind(mut self, operation_kind: ToolOperationKind) -> Self {
        self.operation_kind = operation_kind;
        self
    }

    pub fn with_timeout_scope(mut self, timeout_scope: ToolTimeoutScope) -> Self {
        self.timeout_scope = timeout_scope;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub options: crate::GenerationOptions,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    #[serde(skip)]
    pub extensions: Extensions,
}

/// A provider output item retains its position within one model invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModelOutputItem {
    Message {
        output_index: usize,
        item_id: Option<String>,
        phase: Option<String>,
        content: crate::Content,
    },
    ReasoningSummary {
        output_index: usize,
        text: String,
    },
    ToolCall {
        output_index: usize,
        call: ToolCall,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ModelDisposition {
    Final,
    ToolCalls,
    Continue,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: crate::Content,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_items: Vec<ModelOutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<ModelDisposition>,
    /// Explicit public projection when a host's lossless content contains
    /// internal protocol wrappers. History never reads model_context.
    #[serde(default)]
    pub public_content: Option<crate::Content>,
    /// Only reasoning text actually returned by the provider. Signatures and
    /// opaque continuation data belong exclusively to model_context.
    #[serde(default)]
    pub reasoning: Option<String>,
    pub structured: Option<Value>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub model_context: Option<crate::ModelMessageContext>,
}

/// Optional diagnostics reported by a provider, independent of budget ledger
/// charges. Missing fields remain unknown, including during aggregation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderMetrics {
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl ChatResponse {
    /// Normalize segmented provider output once at the shared model boundary.
    /// Legacy responses without output items keep their existing projection.
    pub fn normalize_output_items(&mut self) -> Result<(), crate::ModelError> {
        if self.output_items.is_empty() {
            return Ok(());
        }
        let index = |item: &ModelOutputItem| match item {
            ModelOutputItem::Message { output_index, .. }
            | ModelOutputItem::ReasoningSummary { output_index, .. }
            | ModelOutputItem::ToolCall { output_index, .. } => *output_index,
        };
        self.output_items.sort_by_key(index);
        if self
            .output_items
            .windows(2)
            .any(|pair| index(&pair[0]) == index(&pair[1]))
        {
            return Err(crate::ModelError::Fatal(
                "duplicate model output index".into(),
            ));
        }
        let item_calls: Vec<_> = self
            .output_items
            .iter()
            .filter_map(|item| match item {
                ModelOutputItem::ToolCall { call, .. } => Some(call.clone()),
                _ => None,
            })
            .collect();
        if self.tool_calls.is_empty() {
            self.tool_calls = item_calls.clone();
        } else if !item_calls.is_empty() && item_calls.len() != self.tool_calls.len() {
            return Err(crate::ModelError::Fatal(
                "model output tool count changed".into(),
            ));
        }
        let mut calls = self.tool_calls.iter();
        for item in &mut self.output_items {
            if let ModelOutputItem::ToolCall { call, .. } = item
                && let Some(normalized) = calls.next()
            {
                *call = normalized.clone();
            }
        }
        let has_content = |content: &crate::Content| {
            content.0.iter().any(|block| match block {
                crate::ContentBlock::Text { text } => !text.trim().is_empty(),
                _ => true,
            })
        };
        let explicit_final = self.output_items.iter().any(|item| matches!(item,
            ModelOutputItem::Message { phase: Some(phase), content, .. } if phase == "final_answer" && has_content(content)));
        let mut content = crate::Content::default();
        let mut selected = 0usize;
        let mut commentary = false;
        let mut other_message = false;
        for item in &self.output_items {
            if let ModelOutputItem::Message {
                phase,
                content: item_content,
                ..
            } = item
                && has_content(item_content)
            {
                commentary |= phase.as_deref() == Some("commentary");
                other_message |= phase.as_deref() != Some("commentary");
                if phase.as_deref() == Some("final_answer") || (!explicit_final && phase.is_none())
                {
                    if selected > 0 {
                        content.0.push(crate::ContentBlock::Text {
                            text: "\n\n".into(),
                        });
                    }
                    content.0.extend(item_content.0.clone());
                    selected += 1;
                }
            }
        }
        self.content = content.clone();
        self.public_content = Some(content);
        self.disposition = Some(if !self.tool_calls.is_empty() {
            ModelDisposition::ToolCalls
        } else if commentary && !other_message {
            ModelDisposition::Continue
        } else {
            ModelDisposition::Final
        });
        Ok(())
    }

    /// Materialize the provider's message and tool-call items in their original
    /// order. Tool results are appended after the whole response is executed.
    pub fn assistant_messages(&self) -> Vec<Message> {
        let mut messages = Vec::<Message>::new();
        let mut calls = self.tool_calls.iter();
        for item in &self.output_items {
            match item {
                ModelOutputItem::Message { phase, content, .. } => {
                    let mut message = Message::assistant(content.clone());
                    message.phase = phase.clone();
                    messages.push(message);
                }
                ModelOutputItem::ToolCall { .. } => {
                    if messages.is_empty() {
                        messages.push(Message::assistant(crate::Content::default()));
                    }
                    if let Some(call) = calls.next() {
                        messages.last_mut().unwrap().tool_calls.push(call.clone());
                    }
                }
                ModelOutputItem::ReasoningSummary { .. } => {}
            }
        }
        if messages.is_empty() {
            messages.push(Message::assistant(self.content.clone()));
        }
        if let Some(last) = messages.last_mut() {
            last.tool_calls.extend(calls.cloned());
            last.structured = self.structured.clone();
            last.model_context = self.model_context.clone();
        }
        messages
    }
    /// Preserve existing provider continuation and the summary's structured
    /// schema. A non-object opaque context cannot carry these optional metrics.
    pub fn with_provider_metrics(mut self, metrics: ProviderMetrics) -> Self {
        let context = self
            .model_context
            .get_or_insert_with(|| crate::ModelMessageContext {
                model: crate::ModelRef::new("framework", "provider-metrics"),
                protocol: "provider-metrics".into(),
                format: "framework.provider-metrics.v1".into(),
                data: serde_json::json!({}),
            });
        if let Some(data) = context.data.as_object_mut() {
            data.insert(
                "frameworkProviderMetrics".into(),
                serde_json::to_value(metrics).expect("provider metrics serialize"),
            );
        }
        self
    }
    pub fn provider_metrics(&self) -> Option<ProviderMetrics> {
        self.model_context
            .as_ref()
            .and_then(|context| context.data.get("frameworkProviderMetrics"))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: text.into().into(),
            ..Self::default()
        }
    }
    pub fn tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorCategory {
    Unavailable,
    Authentication,
    Permission,
    Quota,
    Transient,
    AccountChanged,
    Other,
}

/// Structured provider failure. Messages are raw and must pass through the
/// host's redaction policy before export, just like other model errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModelError {
    /// Opaque transport attempt identity, never a credential. Used to bind recovery to the rejected request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_attempt: Option<String>,
    pub provider: String,
    pub category: ProviderErrorCategory,
    pub http_status: Option<u16>,
    pub business_code: Option<i32>,
    pub error_type: Option<String>,
    pub model: Option<crate::ModelRef>,
    pub message: String,
}

impl std::fmt::Display for ProviderModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = if self.message.trim().is_empty() {
            "model request failed"
        } else {
            &self.message
        };
        write!(f, "{} model error: {}", self.provider, message)?;
        let mut details = Vec::new();
        if let Some(status) = self.http_status {
            details.push(format!("HTTP {status}"));
        }
        if let Some(code) = self.business_code {
            details.push(format!("code {code}"));
        }
        if let Some(kind) = self
            .error_type
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            details.push(format!("type {kind}"));
        }
        if !details.is_empty() {
            write!(f, " [{}]", details.join(", "))?;
        }
        Ok(())
    }
}
impl std::error::Error for ProviderModelError {}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ModelError {
    #[error("invalid structured model output: {0}")]
    InvalidOutput(String),
    #[error(transparent)]
    Provider(Box<ProviderModelError>),
    #[error("model is unavailable: {0}")]
    Unavailable(String),
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error(transparent)]
    State(#[from] crate::StateError),
    #[error("transient model error: {0}")]
    Transient(String),
    #[error("model context overflow: {0}")]
    ContextOverflow(String),
    #[error("model error: {0}")]
    Fatal(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Failed(String),
    #[error("tool effects are indeterminate: {0}")]
    Indeterminate(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ConversationError {
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error("conversation budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("conversation revision conflict")]
    Conflict,
    #[error("conversation error: {0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ContextError {
    #[error("context source refresh required: {source_revision}")]
    RefreshRequired { source_revision: String },
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error("context budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("invalid context history: {0}")]
    InvalidHistory(#[from] ConversationError),
    #[error("context error: {0}")]
    Other(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("operation result is unknown: {0}")]
    Indeterminate(String),
    #[error(transparent)]
    Plugin(#[from] eliteforge_agent_plugin_core::PluginError),
    #[error(transparent)]
    Capability(#[from] crate::CapabilityError),
    #[error("required service is missing: {0}")]
    MissingService(&'static str),
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
    #[error("agent is closed")]
    Closed,
    #[error("session already has an active run: {0}")]
    SessionBusy(String),
    #[error("run was cancelled")]
    Cancelled,
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    ConversationStore(#[from] ConversationError),
    #[error(transparent)]
    Context(#[from] ContextError),
    #[error(transparent)]
    LongTermMemory(#[from] crate::MemoryError),
    #[error("context assembly failed: {source}")]
    ContextAssembly {
        report: Box<crate::ContextFailureReport>,
        #[source]
        source: Box<AgentError>,
    },
    #[error("tool setup error: {0}")]
    ToolSetup(String),
    #[error(transparent)]
    State(#[from] crate::StateError),
    /// The external process/request was definitely not dispatched; no usage is owed.
    #[error("external agent did not start: {0}")]
    ExternalNotStarted(String),
    #[error("run failed: {0}")]
    Run(String),
    /// Execution failed and its cleanup also failed. Both errors remain typed;
    /// the source chain follows the execution error.
    #[error("{primary}; cleanup also failed: {cleanup}")]
    WithCleanup {
        #[source]
        primary: Box<AgentError>,
        cleanup: Box<AgentError>,
    },
}

impl AgentError {
    /// The original execution error, without any attached cleanup failures.
    pub fn primary(&self) -> &Self {
        match self {
            Self::WithCleanup { primary, .. } => primary.primary(),
            Self::ContextAssembly { source, .. } => source.primary(),
            error => error,
        }
    }

    #[doc(hidden)]
    pub fn contains_state_error(&self) -> bool {
        match self {
            Self::State(_) | Self::Model(ModelError::State(_)) => true,
            Self::ContextAssembly { source, .. } => source.contains_state_error(),
            Self::WithCleanup { primary, cleanup } => {
                primary.contains_state_error() || cleanup.contains_state_error()
            }
            _ => false,
        }
    }
}

/// Combine an execution result with cleanup, retaining both typed errors.
pub fn finish_with_cleanup<T>(
    result: Result<T, AgentError>,
    cleanup: Result<(), AgentError>,
) -> Result<T, AgentError> {
    match (result, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(AgentError::WithCleanup {
            primary: Box::new(primary),
            cleanup: Box::new(cleanup),
        }),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    AwaitingInput,
    StepLimit,
    ToolCallLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub output: String,
    pub content: crate::Content,
    pub structured: Option<Value>,
    pub interactions: Vec<crate::InteractionRequest>,
    pub status: RunStatus,
    pub usage: Usage,
    pub steps: usize,
    pub tool_calls: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_session: Option<crate::ExternalSessionRef>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolBatchBudgetPolicy {
    #[default]
    RejectBatch,
    /// Execute the ordered prefix that fits, record non-execution for the rest,
    /// then end the run with ToolCallLimit.
    ExecuteAvailable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLimits {
    /// Maximum model steps; UNLIMITED_COUNT disables this ceiling.
    pub max_steps: usize,
    /// Additional model calls after a response consisting only of commentary.
    #[serde(default = "default_phase_continuations")]
    pub max_phase_continuations: usize,
    /// Maximum tool invocations; UNLIMITED_COUNT disables this ceiling. Zero permits no tools.
    pub max_tool_calls: usize,
    #[serde(default)]
    pub tool_batch_budget_policy: ToolBatchBudgetPolicy,
    /// Permit calls to authorized but currently hidden tools. Stage restrictions,
    /// revocations, schema validation and approval policies still apply.
    #[serde(default)]
    pub allow_unexposed_tools: bool,
    pub max_parallel_tools: usize,
    pub max_model_retries: usize,
    /// Combined main/auxiliary dispatch limit per model step. None is unlimited.
    /// Auxiliary calls reserve one remaining dispatch for the main answer.
    #[serde(default)]
    pub max_model_calls_per_step: Option<usize>,
    pub max_overflow_recoveries: usize,
    pub max_context_messages: usize,
    /// Conservative built-in estimate, not a model-specific tokenizer.
    pub max_context_tokens: usize,
    pub model_timeout: std::time::Duration,
    pub tool_timeout: std::time::Duration,
    pub run_timeout: std::time::Duration,
    /// Cumulative usage ceiling, independent of the context window.
    /// `UNLIMITED_TOKENS` (the default) disables this optional ceiling.
    pub max_total_tokens: u64,
    pub max_model_output_tokens: u32,
    pub cleanup_timeout: std::time::Duration,
    /// Maximum descendants admitted across a root execution tree. Zero disables delegation.
    #[serde(default = "default_child_runs")]
    pub max_child_runs: usize,
    /// Root depth is zero. Each descendant inherits the tighter ancestor limit.
    #[serde(default = "default_child_depth")]
    pub max_delegation_depth: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_phase_continuations: default_phase_continuations(),
            max_tool_calls: 50,
            tool_batch_budget_policy: ToolBatchBudgetPolicy::default(),
            allow_unexposed_tools: false,
            max_parallel_tools: 1,
            max_model_retries: 2,
            max_model_calls_per_step: None,
            max_overflow_recoveries: 1,
            max_context_messages: 256,
            max_context_tokens: 32_000,
            model_timeout: std::time::Duration::from_secs(120),
            tool_timeout: std::time::Duration::from_secs(60),
            run_timeout: std::time::Duration::from_secs(3 * 24 * 60 * 60),
            max_total_tokens: Self::UNLIMITED_TOKENS,
            max_model_output_tokens: 4096,
            cleanup_timeout: std::time::Duration::from_secs(5),
            max_child_runs: default_child_runs(),
            max_delegation_depth: default_child_depth(),
        }
    }
}

fn default_child_runs() -> usize {
    32
}
fn default_phase_continuations() -> usize {
    2
}
fn default_child_depth() -> usize {
    8
}

impl RunLimits {
    /// Explicitly unbounded execution count. Finite child limits still take precedence.
    pub const UNLIMITED_COUNT: usize = usize::MAX;

    /// Whether a finite step/tool ceiling has been reached.
    pub fn count_exhausted(used: usize, limit: usize) -> bool {
        limit != Self::UNLIMITED_COUNT && used >= limit
    }

    /// Model-call ceiling derived only from finite execution limits.
    pub fn model_call_ceiling(&self) -> Option<usize> {
        self.max_model_calls_per_step
            .filter(|per_step| {
                self.max_steps != Self::UNLIMITED_COUNT && *per_step != Self::UNLIMITED_COUNT
            })
            .and_then(|per_step| self.max_steps.checked_mul(per_step))
    }

    /// No cumulative token ceiling. Context and output limits still apply.
    pub const UNLIMITED_TOKENS: u64 = u64::MAX;

    #[doc(hidden)]
    pub fn validate(&self) -> Result<(), AgentError> {
        if [
            self.run_timeout,
            self.model_timeout,
            self.tool_timeout,
            self.cleanup_timeout,
        ]
        .iter()
        .any(|duration| tokio::time::Instant::now().checked_add(*duration).is_none())
        {
            return Err(AgentError::InvalidConfiguration(
                "runtime duration exceeds the supported clock range".into(),
            ));
        }
        if self.max_model_calls_per_step == Some(0)
            || self.max_model_output_tokens == 0
            || self.cleanup_timeout.is_zero()
            || self.model_timeout.is_zero()
            || self.tool_timeout.is_zero()
            || self.run_timeout.is_zero()
            || self.max_total_tokens == 0
            || self.max_steps == 0
            || self.max_parallel_tools == 0
            || self.max_context_messages == 0
            || self.max_context_tokens == 0
        {
            return Err(AgentError::InvalidConfiguration(
                "step, parallelism and context limits must be positive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    #[serde(default)]
    pub annotations: Vec<crate::MessageAnnotation>,
    /// Trusted, opaque host ownership/scope/correlation data, never a prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_provenance: Option<MessageProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_provenance: Option<MessageProvenance>,
    /// Optional named Agent selected by collaboration tools. The host collaboration
    /// runtime must resolve and authorize it, then clear it before executing the run.
    /// A plain Agent cannot resolve application-owned Agent configuration.
    #[serde(default, rename = "agentCode", skip_serializing_if = "Option::is_none")]
    pub agent_code: Option<String>,
    /// Requested child working directory. The host resolves relative paths against
    /// the caller's directory and validates its existing filesystem authority.
    #[serde(
        default,
        rename = "workingDirectory",
        skip_serializing_if = "Option::is_none"
    )]
    pub working_directory: Option<String>,
    /// Host admission identity, shared by control, events and durable run records.
    #[serde(default)]
    pub run_id: Option<RunId>,
    pub session_id: String,
    #[serde(default)]
    pub execution_scope: Option<String>,
    #[serde(default)]
    pub conversation_branch: Option<String>,
    #[serde(default)]
    pub goal_id: Option<String>,
    pub input: crate::Content,
    /// Explicit user selection, independent of the Agent permission set.
    #[serde(default, rename = "requestedSkills")]
    pub requested_skills: Vec<String>,
    pub generation: crate::GenerationOptions,
    pub limits: RunLimits,
    #[serde(skip)]
    pub extensions: Extensions,
    #[serde(skip)]
    pub resume: Option<crate::ResumeRequest>,
    /// Default ReAct phase. Custom loops pass their own phase to ModelInvoker.
    pub model_stage: String,
    #[serde(default)]
    pub model: Option<crate::ModelRef>,
}

impl RunRequest {
    pub fn host_metadata(&self) -> RunHostMetadata {
        RunHostMetadata {
            host_context: self.host_context.clone(),
            message_provenance: self.message_provenance.clone(),
            assistant_provenance: self.assistant_provenance.clone(),
        }
    }
    /// Copies trusted lineage data. Hosts resolving a different target should
    /// assign that target's assistant provenance after inheriting the context.
    pub fn inherit_host_context(&mut self, parent: &RunContext) {
        self.host_context = parent.host_context();
    }
    pub fn scope(&self) -> &str {
        self.execution_scope.as_deref().unwrap_or("main")
    }
    pub fn occupancy_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.session_id.len(),
            self.session_id,
            self.scope()
        )
    }
    pub fn new(session_id: impl Into<String>, input: impl Into<crate::Content>) -> Self {
        Self {
            annotations: Vec::new(),
            agent_code: None,
            working_directory: None,
            host_context: None,
            message_provenance: None,
            assistant_provenance: None,
            session_id: session_id.into(),
            run_id: None,
            execution_scope: None,
            conversation_branch: None,
            goal_id: None,
            input: input.into(),
            requested_skills: Vec::new(),
            generation: crate::GenerationOptions::default(),
            limits: RunLimits::default(),
            extensions: Extensions::default(),
            model_stage: "react".into(),
            model: None,
            resume: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEvent {
    ExternalAgent(crate::ExternalAgentEvent),
    ModelOperationStarted {
        operation: crate::ModelOperation,
        model: crate::ModelRef,
    },
    Output(Box<crate::OutputEvent>),
    ContextAssembled(crate::ContextAssemblyReport),
    ContextAssemblyFailed(crate::ContextFailureReport),
    ModelDispatched(crate::ModelDispatchReport),
    LifecycleHookFailed {
        hook_index: usize,
        phase: crate::RunLifecyclePhase,
        failure: crate::LifecycleHookFailure,
    },
    Collaboration(Box<crate::CollaborationEvent>),
    ModelSelected {
        stage: String,
        model: crate::ModelRef,
        step: usize,
        attempt: usize,
    },
    ModelUsage {
        stage: String,
        model: crate::ModelRef,
        usage: Usage,
    },
    RunStarted {
        run_id: RunId,
        session_id: String,
    },
    ModelStarted {
        step: usize,
        attempt: usize,
    },
    TextDelta(String),
    ReasoningDelta(String),
    ModelRetry {
        step: usize,
        attempt: usize,
    },
    ToolStarted {
        tool_id: String,
        call_id: String,
    },
    ToolFinished {
        tool_id: String,
        call_id: String,
        is_error: bool,
    },
    Steered(String),
    ContextRecovered,
    RunFinished {
        status: RunStatus,
    },
    RunCancelled,
    RunFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    /// Sequence numbers are unique only within this scope and run_id.
    #[serde(default)]
    pub scope: EventScope,
    pub sequence: u64,
    pub timestamp_unix_ms: u64,
    pub run_id: RunId,
    pub session_id: String,
    /// Execution identity for parallel node streams. Absent for run-wide events.
    pub event: AgentEvent,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventScope {
    /// Durable checkpoint journal; retained across executor restarts.
    #[default]
    Journal,
    /// Best-effort stream from one execution attempt, including a resume attempt.
    Live { execution_id: RunId },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunLineage {
    pub root_run_id: RunId,
    pub parent_run_id: Option<RunId>,
    pub depth: usize,
}

#[async_trait]
pub trait ChatStream: Send + Sync {
    async fn text_delta(&self, text: &str);
    async fn reasoning_delta(&self, text: &str);
    /// Message item lifecycle; providers without item boundaries use text_delta.
    async fn message_start(&self, _index: usize, _phase: Option<&str>) {}
    async fn message_delta(&self, _index: usize, text: &str) {
        self.text_delta(text).await;
    }
    async fn message_end(&self, _index: usize, _phase: Option<&str>, _text: &str) {}
    /// Marks a provider tool item before later messages in the same response.
    async fn tool_boundary(&self, _index: usize) {}
}

#[async_trait]
/// Best-effort asynchronous observations. At controlled completion, the runtime
/// drains ordinary events for at most `cleanup_timeout`, then attempts one
/// terminal callback per sink concurrently, each with an additional timeout of
/// `min(cleanup_timeout, 1 second)`. Earlier callbacks cannot run after terminal
/// dispatch. Callback completion is not guaranteed; use `Agent::wait` or a
/// durable checkpoint for authoritative execution state. Callbacks must yield
/// cooperatively so timeout/cancellation can take effect.
#[crate::extension_point(adapter = crate::EventPlugin, contribute)]
pub trait EventSink: Send + Sync {
    /// Ordered live publication, before the best-effort observer queue.
    /// Must be fast, nonblocking, do no IO and never emit another event inline.
    /// Intended for bounded transport handoff. Panics are isolated; implementations
    /// must detect their own capacity failures instead of silently dropping data.
    fn on_published(&self, _context: &RunContext, _event: &EventEnvelope) {}

    /// Called inline only for context reports and final model-dispatch metadata,
    /// while the associated span is alive. Must be fast, nonblocking and do no IO.
    /// It cannot alter execution. Ordinary events still use the bounded async queue.
    fn on_diagnostic(&self, _context: &RunContext, _event: &AgentEvent) {}

    async fn on_event(&self, context: &RunContext, event: &AgentEvent);
}

#[crate::extension_point(select, adapter = crate::AgentLoopPlugin)]
#[async_trait]
pub trait AgentLoop: Send + Sync {
    fn execution_kind(&self) -> &str {
        "CUSTOM"
    }
    /// Loops that own their history externally can run without a ConversationStore.
    fn requires_conversations(&self) -> bool {
        true
    }
    /// Whether unaddressed live steering has defined semantics for this loop.
    fn supports_steering(&self) -> bool {
        true
    }
    /// Validate loop-specific plugin dependencies before accepting any runs.
    fn validate_services(&self, _services: &AgentServices) -> Result<(), AgentError> {
        Ok(())
    }
    async fn validate_services_async(&self, services: &AgentServices) -> Result<(), AgentError> {
        self.validate_services(services)
    }
    fn supports_resume(&self) -> bool {
        false
    }
    fn requires_interactions(&self) -> bool {
        false
    }
    fn supports_tool_policy(&self) -> bool {
        false
    }
    fn requires_state(&self) -> bool {
        false
    }
    fn cooperative_cancellation(&self) -> bool {
        false
    }
    fn requires_model(&self) -> bool {
        false
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum ToolEffect {
    #[default]
    Continue,
    AwaitInput,
    RequestInput(Box<crate::InteractionRequest>),
    /// Expose a subset of the tools authorized by providers for this run.
    ExposeTools(Vec<String>),
    /// Replace the visible subset without granting or revoking authority. Useful
    /// for temporary discovery expansion. IDs must already belong to this run.
    SetExposedTools(Vec<String>),
    /// Permanently revoke these tool IDs for this run. Already dispatched calls
    /// may finish; queued/future calls are checked again before executor entry.
    RevokeTools(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolOutput {
    pub structured: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_reference: Option<Box<crate::ResourceReference>>,
    pub is_error: bool,
    pub content: crate::Content,
    pub effect: ToolEffect,
}

impl ToolOutput {
    pub fn text(content: impl Into<crate::Content>) -> Self {
        Self {
            content: content.into(),
            effect: ToolEffect::Continue,
            structured: None,
            result_reference: None,
            is_error: false,
        }
    }
    pub fn error(content: impl Into<crate::Content>) -> Self {
        Self {
            is_error: true,
            ..Self::text(content)
        }
    }
    pub fn await_input(mut self) -> Self {
        self.effect = ToolEffect::AwaitInput;
        self
    }
    pub fn request_input(mut self, request: crate::InteractionRequest) -> Self {
        self.effect = ToolEffect::RequestInput(Box::new(request));
        self
    }
    pub fn expose_tools(mut self, ids: Vec<String>) -> Self {
        self.effect = ToolEffect::ExposeTools(ids);
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// Opt in to batch-only contracts outside the tool gateway as well.
    fn direct_batch_input(&self) -> bool {
        false
    }
    /// Prepare immutable invocation state before any sibling effects. No locks or
    /// user approval may be held across this boundary.
    async fn prepare(
        &self,
        _arguments: &Value,
        _context: &RunContext,
    ) -> Result<Option<ToolPreparation>, ToolError> {
        Ok(None)
    }
    /// Arguments are JSON Schema validated by the invoker before this call.
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError>;
}

/// Backend-neutral access reservation and durable preparation payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolPreparation {
    pub resources: Vec<ToolResourceAccess>,
    pub state: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResourceAccess {
    pub key: String,
    pub write: bool,
}
impl ToolPreparation {
    pub fn conflicts_with(&self, other: &Self) -> bool {
        self.resources.iter().any(|a| {
            a.write
                && other.resources.iter().any(|b| {
                    b.write
                        && (a.key == b.key
                            || a.key.starts_with(&format!("{}/", b.key))
                            || b.key.starts_with(&format!("{}/", a.key)))
                })
        })
    }
}
/// Host-only invocation state. Never taken from model arguments.
#[derive(Clone)]
pub struct PreparedToolInvocation(pub ToolPreparation);

/// Provider-declared catalog grouping. This metadata is kept apart from the
/// model-facing tool descriptor and does not grant execution authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCategory {
    /// Stable, globally scoped category identity.
    pub code: String,
    /// Localized display names keyed by locale, for example `zh-CN`.
    pub names: std::collections::BTreeMap<String, String>,
}

impl ToolCategory {
    pub fn new(code: impl Into<String>, names: std::collections::BTreeMap<String, String>) -> Self {
        Self {
            code: code.into(),
            names,
        }
    }
    pub fn localized(
        code: impl Into<String>,
        zh: impl Into<String>,
        en: impl Into<String>,
    ) -> Self {
        Self::new(
            code,
            [("zh-CN".into(), zh.into()), ("en-US".into(), en.into())]
                .into_iter()
                .collect(),
        )
    }
}

/// Provenance attached by the host and registration machinery, not by the model.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolSource {
    pub provider_id: Option<String>,
    pub plugin_id: Option<String>,
    pub pack_id: Option<String>,
    pub adapter_id: Option<String>,
}

/// Read-only catalog metadata, independent of an executable tool lease.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCatalogEntry {
    pub descriptor: ToolDescriptor,
    pub domain_code: Option<String>,
    pub category: Option<ToolCategory>,
}

#[cfg(test)]
mod tool_category_tests {
    use super::*;

    #[test]
    fn category_round_trips_without_changing_model_descriptor() {
        let category = ToolCategory::new(
            "example.documents",
            [
                ("zh-CN".into(), "文档工具".into()),
                ("en-US".into(), "Documents".into()),
            ]
            .into_iter()
            .collect(),
        );
        let encoded = serde_json::to_value(&category).unwrap();
        assert_eq!(
            serde_json::from_value::<ToolCategory>(encoded).unwrap(),
            category
        );
        let descriptor = ToolDescriptor::new(
            "example.read",
            "example_read",
            "Read",
            serde_json::json!({}),
        );
        assert!(
            serde_json::to_value(descriptor)
                .unwrap()
                .get("category")
                .is_none()
        );
    }
}

#[derive(Clone)]
pub struct ToolBinding {
    pub descriptor: ToolDescriptor,
    pub executor: Arc<dyn Tool>,
    pub exposed: bool,
    /// Tools run serially unless the host explicitly opts in to parallel calls.
    pub parallel_safe: bool,
    /// Optional opaque host capability-group id. The
    /// gateway inventory search filters by it and hosts render the group list
    /// into their instructions. Purely descriptive discovery metadata: it never
    /// affects admission, policy or exposure.
    pub domain_code: Option<String>,
    /// Optional business category declared by the tool provider.
    pub category: Option<ToolCategory>,
    /// Actual provider and registration provenance, when known.
    pub source: ToolSource,
}

impl ToolBinding {
    pub fn new(descriptor: ToolDescriptor, executor: impl Tool + 'static) -> Self {
        Self {
            descriptor,
            executor: Arc::new(executor),
            exposed: true,
            parallel_safe: false,
            domain_code: None,
            category: None,
            source: ToolSource::default(),
        }
    }
    /// Label this binding with its capability group so `tool_search` can list
    /// that group's tools by id.
    pub fn with_domain_code(mut self, domain_code: impl Into<String>) -> Self {
        self.domain_code = Some(domain_code.into());
        self
    }
    pub fn with_category(mut self, category: ToolCategory) -> Self {
        self.category = Some(category);
        self
    }
    pub fn with_source(mut self, source: ToolSource) -> Self {
        self.source = source;
        self
    }
    pub fn catalog_entry(&self) -> ToolCatalogEntry {
        ToolCatalogEntry {
            descriptor: self.descriptor.clone(),
            domain_code: self.domain_code.clone(),
            category: self.category.clone(),
        }
    }
    pub fn hidden(mut self) -> Self {
        self.exposed = false;
        self
    }
    pub fn parallel_safe(mut self) -> Self {
        self.parallel_safe = true;
        self
    }
}

/// Owns run-local resources. Built-in loops transfer the scope to RunContext so
/// async shutdown runs even when a tool or loop future is cancelled.
pub struct ToolLease {
    pub tools: Vec<ToolBinding>,
    #[doc(hidden)]
    pub resources: eliteforge_agent_plugin_core::ResourceScope,
}
impl ToolLease {
    pub fn new(tools: Vec<ToolBinding>) -> Self {
        Self {
            tools,
            resources: eliteforge_agent_plugin_core::ResourceScope::default(),
        }
    }
    pub fn with_cleanup(mut self, cleanup: impl FnOnce() + Send + Sync + 'static) -> Self {
        self.resources.on_cleanup(cleanup);
        self
    }
    pub fn with_shutdown<F, Fut>(mut self, shutdown: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        self.resources.on_shutdown(shutdown);
        self
    }
    /// Custom loops may close explicitly or transfer with RunContext::own_resources.
    pub async fn close(
        &mut self,
        timeout: std::time::Duration,
    ) -> eliteforge_agent_plugin_core::ShutdownReport {
        self.resources.close_async(timeout).await
    }
}

#[crate::extension_point(adapter = crate::ToolProviderPlugin, contribute)]
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// Static inventory for Framework-wide conflict checks. Dynamic providers
    /// return None and their leased tools are validated by the runtime.
    fn descriptors(&self) -> Option<Vec<ToolDescriptor>> {
        None
    }
    /// Static management inventory. Dynamic providers return None and are
    /// catalogued from actual leases; this never authorizes execution or adds
    /// category metadata to model-facing tool descriptors. Providers may
    /// declare a category per entry or binding; hosts assign provenance from
    /// their registration context. `StaticToolProvider` projects its bindings.
    fn catalog_metadata(&self) -> Option<Vec<ToolCatalogEntry>> {
        self.descriptors().map(|descriptors| {
            descriptors
                .into_iter()
                .map(|descriptor| ToolCatalogEntry {
                    descriptor,
                    domain_code: None,
                    category: None,
                })
                .collect()
        })
    }
    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError>;
}

/// Services frozen at build time. Each run owns all its mutable execution state.
#[derive(Clone)]
pub struct AgentServices {
    pub external_agent: Option<Arc<dyn crate::ExternalAgent>>,
    pub extension_router: Option<Arc<dyn ExtensionRouter>>,
    pub operations: Arc<dyn crate::ModelOperations>,
    pub interactions: Option<crate::InteractionManager>,
    pub lifecycle_hooks: Vec<Arc<dyn crate::RunLifecycleHook>>,
    pub models: Option<Arc<dyn crate::ChatExecutor>>,
    pub tool_invoker: Arc<crate::ToolInvoker>,
    pub run_middleware: Vec<Arc<dyn crate::RunMiddleware>>,
    pub conversations: Arc<dyn crate::ConversationStore>,
    pub long_term_memory: Option<Arc<dyn crate::LongTermMemory>>,
    pub state: Option<Arc<dyn crate::StateStore>>,
    pub tool_policy: Vec<Arc<dyn crate::ToolPolicy>>,
    pub tools: Vec<Arc<dyn ToolProvider>>,
    pub context_sources: Vec<Arc<dyn crate::ContextContributor>>,
}

/// Stable custom contracts expose data, never Any/TypeId or borrowed service objects.
#[async_trait]
pub trait ExtensionRouter: Send + Sync {
    async fn call(
        &self,
        source: &str,
        target: &str,
        operation: &str,
        params: Value,
        run: &RunContext,
        services: &AgentServices,
    ) -> Result<Value, AgentError>;
}

#[doc(hidden)]
#[derive(Clone)]
pub struct ScopedAgentServices(pub Arc<AgentServices>);
#[doc(hidden)]
#[derive(Clone)]
pub struct ScopedRunContext(pub RunContext);
/// W3C propagation data only. Plugin logs, metrics and spans remain local.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceContext {
    pub traceparent: String,
    pub tracestate: String,
}

#[derive(Clone)]
pub struct RunContext {
    pub model: Option<crate::ModelRef>,
    #[doc(hidden)]
    pub limits: RunLimits,
    #[doc(hidden)]
    pub execution_counters: Option<Arc<crate::budget::ExecutionCounters>>,
    #[doc(hidden)]
    pub tool_dispatch_gate: Arc<tokio::sync::RwLock<()>>,
    #[doc(hidden)]
    pub tool_slots: Arc<tokio::sync::Semaphore>,
    pub run_id: RunId,
    pub session_id: String,
    pub extensions: Extensions,
    #[doc(hidden)]
    pub budget_journal: Option<Arc<dyn crate::budget::BudgetJournal>>,
    #[doc(hidden)]
    pub usage: Arc<Mutex<crate::budget::UsageLedger>>,
    #[doc(hidden)]
    pub token_ceiling: u64,
    #[doc(hidden)]
    pub resources: Arc<Mutex<Vec<eliteforge_agent_plugin_core::ResourceScope>>>,
    #[doc(hidden)]
    pub revoked_tools: Arc<Mutex<std::collections::BTreeSet<String>>>,
    #[doc(hidden)]
    pub execution_id: RunId,
    #[doc(hidden)]
    pub lineage: RunLineage,
    #[doc(hidden)]
    pub child_runs: Arc<std::sync::atomic::AtomicUsize>,
    #[doc(hidden)]
    pub max_child_runs: usize,
    #[doc(hidden)]
    pub max_child_depth: usize,
    #[doc(hidden)]
    pub children: CancellationToken,
    #[doc(hidden)]
    pub deadline: tokio::time::Instant,
    #[doc(hidden)]
    pub publication: Arc<Mutex<()>>,
    #[doc(hidden)]
    pub sequence: Arc<std::sync::atomic::AtomicU64>,
    #[doc(hidden)]
    pub observers: Option<Arc<crate::observers::ObserverDispatcher>>,
    #[doc(hidden)]
    pub cancellation: CancellationToken,
    #[doc(hidden)]
    pub steering: Arc<Mutex<SteeringState>>,
    #[doc(hidden)]
    pub events: broadcast::Sender<EventEnvelope>,
    #[doc(hidden)]
    pub sinks: Vec<Arc<dyn EventSink>>,
}

/// Host-owned live input queue, consumed at the same durable boundaries as
/// Agent::steer. `seal_if_empty` must atomically reject future admission when
/// returning no inputs. Return full typed user content, never instructions.
#[async_trait]
pub trait LiveInputSource: Send + Sync {
    /// Structured live messages retain explicit skill selection across durable boundaries.
    async fn drain_inputs(
        &self,
        run: &RunContext,
        seal_if_empty: bool,
    ) -> Result<Vec<LiveInput>, AgentError> {
        Ok(self
            .drain(run, seal_if_empty)
            .await?
            .into_iter()
            .map(LiveInput::from)
            .collect())
    }
    async fn drain(
        &self,
        run: &RunContext,
        seal_if_empty: bool,
    ) -> Result<Vec<crate::Content>, AgentError>;
}
#[derive(Debug, Clone)]
pub struct LiveInput {
    pub annotations: Vec<crate::MessageAnnotation>,
    pub content: crate::Content,
    pub requested_skills: Vec<String>,
    pub provenance: Option<MessageProvenance>,
}
impl From<crate::Content> for LiveInput {
    fn from(content: crate::Content) -> Self {
        Self {
            content,
            annotations: Vec::new(),
            requested_skills: Vec::new(),
            provenance: None,
        }
    }
}
#[derive(Clone)]
pub struct LiveInputSourceService(pub Arc<dyn LiveInputSource>);

#[derive(Default)]
#[doc(hidden)]
pub struct SteeringState {
    pub pending: VecDeque<LiveInput>,
    pub closed: bool,
}

impl RunContext {
    pub fn host_metadata(&self) -> RunHostMetadata {
        self.extensions
            .get::<RunHostMetadataService>()
            .map(|service| service.0.read().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default()
    }
    pub fn host_context(&self) -> Option<Value> {
        self.host_metadata().host_context
    }
    pub fn message_provenance(&self) -> Option<MessageProvenance> {
        self.host_metadata().message_provenance
    }
    pub fn assistant_provenance(&self) -> Option<MessageProvenance> {
        self.host_metadata().assistant_provenance
    }
    /// Limits configured by the host for this run. Descendant or independent
    /// collaboration runs may tighten this policy but must not enlarge it.
    pub fn limits(&self) -> &RunLimits {
        &self.limits
    }

    /// Durable node invocation and the current attempt. Absent outside a node.
    pub fn is_tool_revoked(&self, id: &str) -> bool {
        self.revoked_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
    }
    pub fn revoke_tools(&self, ids: &[String]) -> Result<(), AgentError> {
        record_revocations(&self.revoked_tools, ids)
    }

    pub fn lineage(&self) -> &RunLineage {
        &self.lineage
    }
    pub fn execution_id(&self) -> RunId {
        self.execution_id
    }
    /// Adopt a resource scope immediately after acquisition. The runtime awaits
    /// its graceful shutdown on every controlled exit; Drop executes fallbacks.
    pub fn own_resources(&self, scope: eliteforge_agent_plugin_core::ResourceScope) {
        self.resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(scope);
    }
    #[doc(hidden)]
    pub async fn close_resources(
        &self,
        timeout: std::time::Duration,
    ) -> eliteforge_agent_plugin_core::ShutdownReport {
        let mut scopes =
            std::mem::take(&mut *self.resources.lock().unwrap_or_else(|e| e.into_inner()));
        let mut report = eliteforge_agent_plugin_core::ShutdownReport::default();
        while let Some(mut scope) = scopes.pop() {
            report.merge(scope.close_async(timeout).await);
        }
        report
    }

    pub fn usage(&self) -> Usage {
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .usage
            .clone()
    }
    #[doc(hidden)]
    pub async fn persist_budget(&self) -> Result<(), crate::StateError> {
        if let Some(journal) = &self.budget_journal {
            journal
                .save(self.usage.clone(), self.child_runs.clone())
                .await?;
        }
        Ok(())
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
    pub fn check_cancelled(&self) -> Result<(), AgentError> {
        if self.is_cancelled() {
            Err(AgentError::Cancelled)
        } else {
            Ok(())
        }
    }
    pub fn take_steering(&self) -> Vec<String> {
        self.take_steering_content()
            .into_iter()
            .map(|content| content.protocol_text())
            .collect()
    }
    pub fn take_steering_content(&self) -> Vec<crate::Content> {
        self.take_steering_inputs()
            .into_iter()
            .map(|input| input.content)
            .collect()
    }
    pub fn take_steering_inputs(&self) -> Vec<LiveInput> {
        self.steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .drain(..)
            .collect()
    }
    #[doc(hidden)]
    pub fn pending_or_seal(&self) -> Vec<String> {
        self.pending_content_or_seal()
            .into_iter()
            .map(|content| content.protocol_text())
            .collect()
    }
    #[doc(hidden)]
    pub fn pending_content_or_seal(&self) -> Vec<crate::Content> {
        self.pending_inputs_or_seal()
            .into_iter()
            .map(|input| input.content)
            .collect()
    }
    pub fn pending_inputs_or_seal(&self) -> Vec<LiveInput> {
        let mut state = self.steering.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending.is_empty() {
            state.closed = true;
        }
        state.pending.drain(..).collect()
    }
    #[doc(hidden)]
    pub fn seal_steering(&self) {
        self.steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
    }
    pub async fn emit(&self, event: AgentEvent) {
        // Publication is live-only. Durable state/history is committed by the
        // execution strategy at semantic boundaries, never one row per delta.
        self.publish(event);
    }
    pub fn emit_output(&self, event: crate::OutputEvent) {
        let message_id = match &event {
            crate::OutputEvent::MessageStarted { message_id } => Some(message_id.clone()),
            _ => None,
        };
        self.publish(AgentEvent::Output(Box::new(event)));
        if let Some(message_id) = message_id
            && let Some(provenance) = self.assistant_provenance()
        {
            self.publish(AgentEvent::Output(Box::new(crate::OutputEvent::Custom {
                name: "framework.message.provenance".into(),
                value: serde_json::json!({"messageId":message_id,"provenance":provenance,"sessionId":self.session_id,"runId":self.run_id}),
            })));
        }
    }
    #[doc(hidden)]
    pub fn publish(&self, event: AgentEvent) {
        if matches!(
            event,
            AgentEvent::ContextAssembled(_)
                | AgentEvent::ContextAssemblyFailed(_)
                | AgentEvent::ModelDispatched(_)
        ) {
            for sink in &self.sinks {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    sink.on_diagnostic(self, &event)
                }));
            }
        }
        let _publication = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let envelope = self.envelope(event.clone());
        for sink in &self.sinks {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.on_published(self, &envelope)
            }));
        }
        let _ = self.events.send(envelope);
        if let Some(observers) = &self.observers {
            observers.enqueue(event);
        }
    }
    #[doc(hidden)]
    pub fn envelope(&self, event: AgentEvent) -> EventEnvelope {
        EventEnvelope {
            schema_version: 1,
            scope: EventScope::Live {
                execution_id: self.execution_id,
            },
            sequence: self
                .sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            timestamp_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            run_id: self.run_id,
            session_id: self.session_id.clone(),
            event,
        }
    }
    pub fn dropped_observations(&self) -> u64 {
        self.observers.as_ref().map_or(0, |o| o.dropped())
    }
}

#[async_trait]
impl ChatStream for RunContext {
    async fn text_delta(&self, text: &str) {
        self.emit(AgentEvent::TextDelta(text.to_owned())).await;
    }
    async fn reasoning_delta(&self, text: &str) {
        self.emit(AgentEvent::ReasoningDelta(text.to_owned())).await;
    }
}

#[doc(hidden)]
pub fn record_revocations(
    registry: &Mutex<std::collections::BTreeSet<String>>,
    ids: &[String],
) -> Result<(), AgentError> {
    let mut revoked = registry.lock().unwrap_or_else(|e| e.into_inner());
    if ids.iter().any(|id| id.is_empty() || id.len() > 1024) {
        return Err(AgentError::InvalidConfiguration(
            "revoked tool IDs must be nonempty and at most 1024 bytes".into(),
        ));
    }
    let additional: std::collections::BTreeSet<_> = ids
        .iter()
        .filter(|id| !revoked.contains(*id))
        .cloned()
        .collect();
    if revoked.len().saturating_add(additional.len()) > 4096 {
        return Err(AgentError::Run(
            "run tool revocation capacity exceeded".into(),
        ));
    }
    revoked.extend(additional);
    Ok(())
}

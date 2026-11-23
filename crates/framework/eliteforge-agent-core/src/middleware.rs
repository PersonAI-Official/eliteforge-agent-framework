//! Ordered call interception. Terminal validation cannot be bypassed by mutation.
use crate::*;
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelDispatchReport {
    pub model: ModelRef,
    pub stage: String,
    pub step: usize,
    pub attempt: usize,
    pub input_capacity: usize,
    pub estimated_input_tokens: u64,
    pub message_count: usize,
    pub tool_count: usize,
    pub output_token_limit: u32,
}

pub struct ModelInvocationContext<'a> {
    pub model: &'a ModelRef,
    pub stage: &'a str,
    pub step: usize,
    pub attempt: usize,
    pub run: &'a RunContext,
}

#[crate::extension_point(adapter = crate::ModelMiddlewarePlugin, contribute)]
#[async_trait]
pub trait ModelMiddleware: Send + Sync {
    async fn intercept(
        &self,
        request: ChatRequest,
        context: &ModelInvocationContext<'_>,
        next: ModelNext<'_>,
    ) -> Result<ChatResponse, ModelError>;
}

pub struct ModelNext<'a> {
    #[doc(hidden)]
    pub middleware: &'a [Arc<dyn ModelMiddleware>],
    #[doc(hidden)]
    pub provider: &'a dyn ChatModel,
    #[doc(hidden)]
    pub stream: &'a dyn ChatStream,
    #[doc(hidden)]
    pub budget: ContextBudget,
    #[doc(hidden)]
    pub descriptor: &'a ModelDescriptor,
    #[doc(hidden)]
    pub estimator: &'a dyn TokenEstimator,
    #[doc(hidden)]
    pub max_total_tokens: u64,
    #[doc(hidden)]
    pub default_output_tokens: u32,
    #[doc(hidden)]
    pub run: &'a RunContext,
    #[doc(hidden)]
    pub target: &'a ModelRef,
    #[doc(hidden)]
    pub authorized_tools: &'a [ToolDescriptor],
}

impl ModelNext<'_> {
    pub async fn run(
        self,
        mut request: ChatRequest,
        context: &ModelInvocationContext<'_>,
    ) -> Result<ChatResponse, ModelError> {
        if let Some((first, remaining)) = self.middleware.split_first() {
            first
                .intercept(
                    request,
                    context,
                    ModelNext {
                        middleware: remaining,
                        ..self
                    },
                )
                .await
        } else {
            if context.model != self.target {
                return Err(ModelError::Fatal(
                    "middleware cannot change the selected model".into(),
                ));
            }
            if request
                .tools
                .iter()
                .any(|tool| !self.authorized_tools.contains(tool))
            {
                return Err(ModelError::Fatal(
                    "middleware cannot introduce unauthorized tool contracts".into(),
                ));
            }
            request.options.max_output_tokens = Some(
                self.descriptor
                    .output_limit(self.default_output_tokens, &request.options),
            );
            let mut budget = self.budget;
            if let Some(window) = self.descriptor.context_window_tokens {
                let reserve = request
                    .options
                    .max_output_tokens
                    .unwrap_or(self.default_output_tokens) as usize;
                budget.max_tokens = budget
                    .max_tokens
                    .min(window.checked_sub(reserve).ok_or_else(|| {
                        ModelError::Fatal("output reserve exceeds context window".into())
                    })?);
            }
            // Project structured comments only for the provider call; checkpoints
            // and conversation/history records retain the original body and metadata.
            for message in &mut request.messages {
                if let Some(value) = annotation_model_context(&message.annotations) {
                    message.content.0.push(ContentBlock::Json { value });
                    message.annotations.clear();
                }
            }
            validate_model_request(
                &request,
                budget,
                self.descriptor,
                self.target,
                self.estimator,
            )
            .await?;
            let input_tokens = self
                .estimator
                .request(self.target, &request)
                .await?
                .min(u64::MAX as usize) as u64;
            let (mut permit, output_tokens) = crate::budget::UsagePermit::reserve(
                self.run.usage.clone(),
                input_tokens,
                request
                    .options
                    .max_output_tokens
                    .unwrap_or(self.default_output_tokens),
                self.max_total_tokens.min(self.run.token_ceiling),
            )?;
            request.options.max_output_tokens = Some(output_tokens);
            let prepared = async {
                validate_model_request(
                    &request,
                    budget,
                    self.descriptor,
                    self.target,
                    self.estimator,
                )
                .await?;
                let final_input_tokens = self
                    .estimator
                    .request(self.target, &request)
                    .await?
                    .min(u64::MAX as usize) as u64;
                if final_input_tokens > input_tokens {
                    return Err(ModelError::Fatal(
                        "token estimator increased input cost after reducing output allowance"
                            .into(),
                    ));
                }
                Ok(final_input_tokens)
            }
            .await;
            let final_input_tokens = match prepared {
                Ok(tokens) => tokens,
                Err(error) => {
                    permit.release_unspent();
                    self.run.persist_budget().await?;
                    return Err(error);
                }
            };
            let report = ModelDispatchReport {
                model: self.target.clone(),
                stage: context.stage.into(),
                step: context.step,
                attempt: context.attempt,
                input_capacity: budget.max_tokens,
                estimated_input_tokens: final_input_tokens,
                message_count: request.messages.len(),
                tool_count: request.tools.len(),
                output_token_limit: output_tokens,
            };
            let options = request.options.clone();
            if let Some(limit) = self.run.extensions.get::<crate::ModelCallLimit>()
                && let Err(error) = limit.admit()
            {
                permit.release_unspent();
                self.run.persist_budget().await?;
                return Err(error);
            }
            if let Some(budget) = self.run.extensions.get::<crate::ContinuationBudget>()
                && let Err(error) = budget.admit_model_call()
            {
                permit.release_unspent();
                self.run.persist_budget().await?;
                return Err(error);
            }
            self.run.persist_budget().await?;
            if let Some(counter) = self.run.extensions.get::<crate::ModelDispatchCounter>() {
                counter.record();
            }
            self.run.emit(AgentEvent::ModelDispatched(report)).await;
            request
                .extensions
                .insert(crate::ScopedRunContext(self.run.clone()));
            let response = self
                .provider
                .generate(&self.target.model, request, self.stream)
                .await;
            let mut response = match response {
                Ok(response) => {
                    let settlement = permit.settle(
                        &response.usage,
                        self.max_total_tokens.min(self.run.token_ceiling),
                    );
                    self.run.persist_budget().await?;
                    settlement?;
                    response
                }
                Err(error) => {
                    drop(permit);
                    self.run.persist_budget().await?;
                    return Err(error);
                }
            };
            validate_model_response(&mut response, &options)?;
            if response
                .content
                .modalities()
                .iter()
                .any(|m| !self.descriptor.output_modalities.contains(m))
            {
                return Err(ModelError::Fatal(
                    "model returned an undeclared output modality".into(),
                ));
            }
            Ok(response)
        }
    }
}

#[doc(hidden)]
pub async fn validate_model_request(
    request: &ChatRequest,
    budget: ContextBudget,
    descriptor: &ModelDescriptor,
    model: &ModelRef,
    estimator: &dyn TokenEstimator,
) -> Result<(), ModelError> {
    request
        .options
        .validate()
        .map_err(|e| ModelError::Fatal(e.to_string()))?;
    crate::conversation::validate_messages(&request.messages)
        .map_err(|e| ModelError::Fatal(e.to_string()))?;
    let tokens = estimator.request(model, request).await?;
    if request.messages.len() > budget.max_messages || tokens > budget.max_tokens {
        return Err(ModelError::ContextOverflow(
            "composed model request exceeds budget".into(),
        ));
    }
    if !request.tools.is_empty() && descriptor.supports_tools == Some(false) {
        return Err(ModelError::Fatal(
            "selected model does not support tools".into(),
        ));
    }
    if request.options.output_schema.is_some()
        && descriptor.supports_structured_output == Some(false)
    {
        return Err(ModelError::Fatal(
            "selected model does not support structured output".into(),
        ));
    }
    if request
        .messages
        .iter()
        .flat_map(|m| m.content.modalities())
        .any(|m| !descriptor.input_modalities.contains(&m))
    {
        return Err(ModelError::Fatal(
            "selected model does not support the input modalities".into(),
        ));
    }
    let mut names = std::collections::HashSet::new();
    for tool in &request.tools {
        if !names.insert(&tool.call_name) {
            return Err(ModelError::Fatal("duplicate model tool name".into()));
        }
        compile_schema(&tool.input_schema).map_err(|e| ModelError::Fatal(e.to_string()))?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn validate_model_response(
    response: &mut ChatResponse,
    options: &GenerationOptions,
) -> Result<(), ModelError> {
    response.normalize_output_items()?;
    if response.disposition != Some(crate::ModelDisposition::Continue)
        && response.tool_calls.is_empty()
        && let Some(output) = &options.output_schema
    {
        let value = if response.content.is_empty() {
            response.structured.clone().ok_or_else(|| {
                ModelError::InvalidOutput("model did not return structured output".into())
            })?
        } else {
            match response.content.0.as_slice() {
                [ContentBlock::Json { value }] => value.clone(),
                blocks
                    if blocks
                        .iter()
                        .all(|b| matches!(b, ContentBlock::Text { .. })) =>
                {
                    serde_json::from_str(&response.content.text()).map_err(|_| {
                        ModelError::InvalidOutput("model did not return valid JSON".into())
                    })?
                }
                _ => {
                    return Err(ModelError::InvalidOutput(
                        "structured output contains unsupported mixed content".into(),
                    ));
                }
            }
        };
        if response
            .structured
            .as_ref()
            .is_some_and(|structured| structured != &value)
        {
            return Err(ModelError::InvalidOutput(
                "structured output disagrees with JSON content".into(),
            ));
        }
        if !compile_schema(&output.schema)
            .map_err(|e| ModelError::Fatal(e.to_string()))?
            .is_valid(&value)
        {
            return Err(ModelError::InvalidOutput(
                "model output does not match output schema".into(),
            ));
        }
        response.structured = Some(value);
    }
    Ok(())
}

/// Wraps one run execution. Inputs are immutable; `next` preserves the admitted
/// request, identity, limits and cancellation context. Use `finish_with_cleanup`
/// when an interceptor has both an execution error and a cleanup error.
#[crate::extension_point(adapter = crate::RunMiddlewarePlugin, contribute)]
#[async_trait]
pub trait RunMiddleware: Send + Sync {
    async fn intercept(
        &self,
        request: &RunRequest,
        context: &RunContext,
        next: RunNext<'_>,
    ) -> Result<RunResult, AgentError>;
}

pub struct RunNext<'a> {
    #[doc(hidden)]
    pub middleware: &'a [Arc<dyn RunMiddleware>],
    #[doc(hidden)]
    pub agent_loop: &'a dyn AgentLoop,
    #[doc(hidden)]
    pub services: &'a AgentServices,
    #[doc(hidden)]
    pub request: &'a RunRequest,
    #[doc(hidden)]
    pub context: &'a RunContext,
}
impl RunNext<'_> {
    pub async fn run(self) -> Result<RunResult, AgentError> {
        use futures::FutureExt;
        let context = self.context;
        let grace = self.request.limits.cleanup_timeout;
        let cooperative = !self.middleware.is_empty() || self.agent_loop.cooperative_cancellation();
        let mut result = {
            let work = std::panic::AssertUnwindSafe(async {
                if let Some((first, remaining)) = self.middleware.split_first() {
                    first
                        .intercept(
                            self.request,
                            context,
                            RunNext {
                                middleware: remaining,
                                ..self
                            },
                        )
                        .await
                } else {
                    context.check_cancelled()?;
                    if tokio::time::Instant::now() >= context.deadline {
                        context.cancellation.cancel();
                        return Err(AgentError::Run(
                            "run deadline exceeded before dispatch".into(),
                        ));
                    }
                    self.agent_loop
                        .run(self.request, context, self.services)
                        .await
                }
            })
            .catch_unwind();
            tokio::pin!(work);
            let interrupted = tokio::select! {
                biased;
                _ = context.cancelled() => Err(AgentError::Cancelled),
                result = &mut work => Ok(result),
                _ = tokio::time::sleep_until(context.deadline) => {
                    context.cancellation.cancel();
                    Err(AgentError::Run("run deadline exceeded".into()))
                },
            };
            match interrupted {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(AgentError::Run("plugin panicked during execution".into())),
                Err(error) if cooperative => match tokio::time::timeout(grace, &mut work).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(AgentError::Run(
                        "plugin panicked during cancellation cleanup".into(),
                    )),
                    Err(_) => Err(error),
                },
                Err(error) => Err(error),
            }
        };
        context.children.cancel();
        let cleanup = context.close_resources(grace).await;
        if !cleanup.is_success() {
            result = finish_with_cleanup(
                result,
                Err(AgentError::Run(format!(
                    "run resource shutdown failed: {cleanup:?}"
                ))),
            );
        }
        result
    }
}

/// Trusted invocation identity supplied by the invoker, available to executors
/// through `RunContext.extensions`. Each concurrent call receives its own value.
/// Persist `invocation_id` when a tool needs an idempotency key across recovery.
#[derive(Debug, Clone)]
pub struct ToolCallIdentity {
    pub invocation_id: String,
    pub provider_call_id: String,
    pub tool_id: String,
}

pub struct ToolInvocationContext<'a> {
    pub invocation_id: &'a str,
    pub call: &'a ToolCall,
    pub descriptor: &'a ToolDescriptor,
    pub run: &'a RunContext,
}

/// Host projection of a confirmed result, including rejected/invalid calls.
/// Called at each completion before the result enters durable history; parallel
/// calls may complete out of model-call order. Native tool effects have already
/// been resolved, so a rejected exposure is observed as a failure. Effects added
/// by this processor are checked again before persistence. On recovery it can
/// run again; implementations must use invocation_id for idempotent external
/// persistence. It cannot turn a failure into success.
#[async_trait]
pub trait ToolResultProcessor: Send + Sync {
    async fn process(
        &self,
        invocation: &ToolInvocationContext<'_>,
        output: ToolOutput,
    ) -> Result<ToolOutput, AgentError>;
}
pub struct ToolResultProcessorService(pub Arc<dyn ToolResultProcessor>);

/// Intercepts a single immutable tool invocation. A denial returns an error
/// without calling next. Execution failures cannot be converted to success;
/// an interceptor error after successful dispatch is conservatively indeterminate.
#[crate::extension_point(adapter = crate::ToolMiddlewarePlugin, contribute)]
#[async_trait]
pub trait ToolMiddleware: Send + Sync {
    async fn intercept(
        &self,
        context: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError>;
}

pub struct ToolNext<'a> {
    middleware: &'a [Arc<dyn ToolMiddleware>],
    invocation: &'a ToolInvocationContext<'a>,
    binding: &'a ToolBinding,
    execution: &'a ToolExecution,
    deadline: tokio::time::Instant,
    cleanup_timeout: std::time::Duration,
}
#[derive(Default)]
struct ToolExecution {
    dispatched: std::sync::atomic::AtomicBool,
    failure: std::sync::Mutex<Option<ToolError>>,
}
impl ToolExecution {
    fn interrupted(&self, message: &str) -> ToolError {
        if self.dispatched.load(std::sync::atomic::Ordering::Acquire) {
            ToolError::Indeterminate(message.into())
        } else {
            ToolError::Failed(message.into())
        }
    }
}
impl ToolNext<'_> {
    pub async fn run(self) -> Result<ToolOutput, ToolError> {
        use futures::FutureExt;
        use std::sync::atomic::Ordering;
        let context = self.invocation.run;
        let wrapped = !self.middleware.is_empty();
        let result = {
            let work = std::panic::AssertUnwindSafe(async {
                if let Some((first, remaining)) = self.middleware.split_first() {
                    first
                        .intercept(
                            self.invocation,
                            ToolNext {
                                middleware: remaining,
                                ..self
                            },
                        )
                        .await
                } else {
                    if context.is_cancelled() || tokio::time::Instant::now() >= self.deadline {
                        return Err(ToolError::Failed("tool interrupted before dispatch".into()));
                    }
                    if context.is_tool_revoked(&self.binding.descriptor.id) {
                        return Err(ToolError::Failed("tool was revoked before dispatch".into()));
                    }
                    self.execution.dispatched.store(true, Ordering::Release);
                    let mut invocation_run = context.clone();
                    invocation_run.extensions.insert(ToolCallIdentity {
                        invocation_id: self.invocation.invocation_id.to_owned(),
                        provider_call_id: self.invocation.call.id.clone(),
                        tool_id: self.binding.descriptor.id.clone(),
                    });
                    self.binding
                        .executor
                        .call(self.invocation.call.arguments.clone(), &invocation_run)
                        .await
                }
            })
            .catch_unwind();
            tokio::pin!(work);
            let interrupted = tokio::select! {
                biased;
                _ = context.cancelled() => Err(self.execution.interrupted("tool cancelled")),
                result = &mut work => Ok(result),
                _ = tokio::time::sleep_until(self.deadline) => Err(self.execution.interrupted("tool timed out")),
            };
            match interrupted {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(self.execution.interrupted("tool or middleware panicked")),
                Err(error) if wrapped => {
                    match tokio::time::timeout(self.cleanup_timeout, &mut work).await {
                        Ok(Ok(Ok(_))) => Err(self
                            .execution
                            .interrupted("tool middleware interrupted during cleanup")),
                        Ok(Ok(Err(error))) => Err(error),
                        Ok(Err(_)) => Err(self
                            .execution
                            .interrupted("tool middleware panicked during cleanup")),
                        Err(_) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        };
        let mut failure = self
            .execution
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &*failure {
            return Err(error.clone());
        }
        let result = match result {
            Err(ToolError::Failed(message))
                if wrapped && self.execution.dispatched.load(Ordering::Acquire) =>
            {
                Err(ToolError::Indeterminate(format!(
                    "tool middleware failed after dispatch: {message}"
                )))
            }
            Ok(_) if context.is_cancelled() => Err(self.execution.interrupted("tool cancelled")),
            result => result,
        };
        if let Err(error) = &result {
            *failure = Some(error.clone());
        }
        result
    }
}

#[derive(Clone, Default)]
pub struct ToolInvoker {
    middleware: Vec<Arc<dyn ToolMiddleware>>,
}

impl ToolInvoker {
    pub fn new(middleware: Vec<Arc<dyn ToolMiddleware>>) -> Self {
        Self { middleware }
    }

    /// Narrow the original provider catalog through the run's installed
    /// policies. Resources must already belong to RunContext before this call.
    pub async fn admit_binding(
        &self,
        binding: &mut ToolBinding,
        context: &RunContext,
    ) -> Result<bool, AgentError> {
        context.check_cancelled()?;
        if let Some(services) = context.extensions.get::<ScopedAgentServices>() {
            for policy in &services.0.tool_policy {
                let decision = tokio::select! {
                    biased;
                    _ = context.cancelled() => return Err(AgentError::Cancelled),
                    result = tokio::time::timeout_at(context.deadline, policy.admit_binding(binding, context)) =>
                        result.map_err(|_| AgentError::Run("tool catalog admission timed out".into()))??,
                };
                match decision {
                    ToolBindingAdmission::Allow => {}
                    ToolBindingAdmission::Hidden => binding.exposed = false,
                    ToolBindingAdmission::Exclude => return Ok(false),
                }
            }
        }
        Ok(true)
    }

    /// Publish only the final catalog, never an intermediate policy decision.
    pub async fn catalog_ready(
        &self,
        bindings: &[ToolBinding],
        context: &RunContext,
    ) -> Result<(), AgentError> {
        context.check_cancelled()?;
        if let Some(services) = context.extensions.get::<ScopedAgentServices>() {
            for policy in &services.0.tool_policy {
                tokio::select! {
                    biased;
                    _ = context.cancelled() => return Err(AgentError::Cancelled),
                    result = tokio::time::timeout_at(context.deadline, policy.catalog_ready(bindings, context)) =>
                        result.map_err(|_| AgentError::Run("tool catalog observation timed out".into()))??,
                }
            }
        }
        Ok(())
    }

    pub async fn invoke(
        &self,
        invocation_id: &str,
        call: &ToolCall,
        binding: &ToolBinding,
        context: &RunContext,
        timeout: std::time::Duration,
    ) -> Result<ToolOutput, ToolError> {
        if binding.executor.direct_batch_input()
            && binding.descriptor.input_mode == crate::ToolInputMode::Batch
            && context.extensions.get::<crate::ToolBatchItem>().is_none()
        {
            let items = crate::tool_batch_items(&call.arguments).map_err(ToolError::Failed)?;
            let mut prepared = Vec::new();
            for arguments in items {
                prepared.push(
                    binding
                        .executor
                        .prepare(arguments, context)
                        .await
                        .map_err(|e| e.to_string()),
                );
            }
            let mut outputs = Vec::new();
            for (index, arguments) in items.iter().enumerate() {
                let invocation = crate::RunId::new_v4();
                let identity = crate::ToolBatchItem {
                    invocation_id: invocation,
                    call_id: invocation.to_string(),
                    parent_call_id: call.id.clone(),
                    parent_invocation_id: call.invocation_id,
                    index,
                };
                let mut child = call.clone();
                child.arguments = arguments.clone();
                child.id = identity.call_id.clone();
                child.invocation_id = invocation;
                let mut run = context.clone();
                run.extensions.insert(identity.clone());
                if let Ok(Some(state)) = &prepared[index] {
                    run.extensions
                        .insert(crate::PreparedToolInvocation(state.clone()));
                }
                let overlap = prepared[index]
                    .as_ref()
                    .ok()
                    .and_then(Option::as_ref)
                    .is_some_and(|a| {
                        prepared.iter().enumerate().any(|(j, b)| {
                            j != index
                                && b.as_ref()
                                    .ok()
                                    .and_then(Option::as_ref)
                                    .is_some_and(|b| a.conflicts_with(b))
                        })
                    });
                let output = if let Err(error) = &prepared[index] {
                    ToolOutput::error(error)
                } else if overlap {
                    ToolOutput::error("notApplied: overlapping batch writers")
                } else {
                    match Box::pin(self.invoke(
                        &invocation.to_string(),
                        &child,
                        binding,
                        &run,
                        timeout,
                    ))
                    .await
                    {
                        Ok(output) => output,
                        Err(error) => ToolOutput::error(error.to_string()),
                    }
                };
                outputs.push(crate::ToolBatchItemResult::finished(identity, &output));
            }
            return Ok(crate::tool_batch_output(outputs));
        }
        let run_scoped = binding.descriptor.timeout_scope == crate::ToolTimeoutScope::Run;
        if context.is_tool_revoked(&binding.descriptor.id) {
            return Err(ToolError::Failed("tool was revoked".into()));
        }
        let validator = compile_schema(&binding.descriptor.input_schema)
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        if !validator.is_valid(&call.arguments) {
            return Err(ToolError::Failed(
                "tool arguments do not match input schema".into(),
            ));
        }
        let mut prepared_context = context.clone();
        if prepared_context
            .extensions
            .get::<crate::PreparedToolInvocation>()
            .is_none()
        {
            let preparation = tokio::select! {
                biased;
                _ = context.cancelled() => return Err(ToolError::Failed("tool preparation cancelled".into())),
                result = tokio::time::timeout_at(context.deadline, binding.executor.prepare(&call.arguments, context)) => result.map_err(|_| ToolError::Failed("tool preparation deadline exceeded".into()))??,
            };
            if let Some(preparation) = preparation {
                prepared_context
                    .extensions
                    .insert(crate::PreparedToolInvocation(preparation));
            }
        }
        let context = &prepared_context;
        // This gate belongs to the run, so parallel DAG nodes obey the same
        // tool-safety contract as calls within a single ReAct batch. Children
        // own separate gates to permit a tool to await a delegated Agent.
        let admission = async {
            let gate = if binding.parallel_safe {
                futures::future::Either::Left(context.tool_dispatch_gate.clone().read_owned().await)
            } else {
                futures::future::Either::Right(
                    context.tool_dispatch_gate.clone().write_owned().await,
                )
            };
            let slot = context
                .tool_slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ToolError::Failed("tool admission is closed".into()))?;
            Ok::<_, ToolError>((gate, slot))
        };
        let _admission = tokio::select! {
            biased;
            _ = context.cancelled() => return Err(ToolError::Failed("tool cancelled before dispatch".into())),
            result = tokio::time::timeout_at(
                if run_scoped { context.deadline } else { tokio::time::Instant::now() + timeout },
                admission,
            ) => result
                .map_err(|_| ToolError::Failed("tool admission timed out before dispatch".into()))??,
        };
        let invocation = ToolInvocationContext {
            invocation_id,
            call,
            descriptor: &binding.descriptor,
            run: context,
        };
        let execution = ToolExecution::default();
        let result = ToolNext {
            middleware: &self.middleware,
            invocation: &invocation,
            binding,
            execution: &execution,
            deadline: if run_scoped {
                context.deadline
            } else {
                tokio::time::Instant::now() + timeout
            },
            cleanup_timeout: if run_scoped {
                context.limits().cleanup_timeout
            } else {
                timeout
            },
        }
        .run()
        .await;
        if let Ok(output) = &result
            && let ToolEffect::RevokeTools(ids) = &output.effect
        {
            context
                .revoke_tools(ids)
                .map_err(|e| ToolError::Indeterminate(e.to_string()))?;
        }
        result
    }
}

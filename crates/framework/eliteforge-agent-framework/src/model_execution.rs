//! Durable ReAct model and tool execution.
use crate::*;
use async_trait::async_trait;
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
pub(crate) struct ModelExecutionLoop {
    lease_duration: Duration,
}
impl ModelExecutionLoop {
    pub fn new() -> Self {
        Self {
            lease_duration: Duration::from_secs(30),
        }
    }
}

#[async_trait]
impl AgentLoop for ModelExecutionLoop {
    fn execution_kind(&self) -> &str {
        "REACT"
    }
    fn supports_resume(&self) -> bool {
        true
    }
    fn requires_interactions(&self) -> bool {
        true
    }
    fn supports_tool_policy(&self) -> bool {
        true
    }
    fn requires_model(&self) -> bool {
        true
    }
    fn requires_state(&self) -> bool {
        true
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let store = services
            .state
            .clone()
            .ok_or(AgentError::MissingService("StateStore"))?;
        let mut conversation = Some(
            services
                .conversations
                .open_branch(&ConversationKey {
                    session_id: request.session_id.clone(),
                    branch: request
                        .conversation_branch
                        .clone()
                        .unwrap_or_else(|| "main".into()),
                })
                .await?,
        );
        let mut initial_tools = if request.resume.is_none() {
            Some(crate::react::ToolSet::load(&services.tools, context).await?)
        } else {
            None
        };
        let mut checkpoint = if let Some(resume) = &request.resume {
            let checkpoint = store.load(resume.run_id).await?;
            if checkpoint.session_id != request.session_id
                || checkpoint.execution_scope != request.scope()
                || checkpoint.loop_id != "framework.react"
                || checkpoint.loop_version != 1
            {
                return Err(AgentError::InvalidConfiguration(
                    "resume identity or ReAct version mismatch".into(),
                ));
            }
            checkpoint
        } else {
            let previous = store
                .latest_scope(&request.session_id, request.scope())
                .await?;
            let mut saved_request = request.clone();
            saved_request.resume = None;
            saved_request.extensions = Extensions::default();
            let initial_snapshot = if let Some(session) = &mut conversation {
                let snapshot = session.snapshot().await?;
                if let Some(previous) = &previous
                    && previous.loop_id == "framework.react"
                    && previous.status.is_terminal()
                {
                    let prior: ReactState =
                        serde_json::from_value(previous.state.clone()).map_err(state_encoding)?;
                    if prior.conversation_generation.as_deref() == Some(&snapshot.generation) {
                        sync_transcript(
                            session.as_mut(),
                            &previous.transcript,
                            prior.conversation_generation.as_deref(),
                        )
                        .await?;
                    }
                }
                Some(session.snapshot().await?)
            } else {
                None
            };
            let state = ReactState {
                request: saved_request,
                conversation_generation: initial_snapshot.as_ref().map(|s| s.generation.clone()),
                steps: 0,
                phase_continuations: 0,
                tool_calls: 0,
                tool_batch_limit_reached: false,
                cursor: ReactCursor::Model,
                pending_model_response: None,
                pending_assistant_messages: Vec::new(),
                visible_tools: initial_tools
                    .as_ref()
                    .expect("new run catalog")
                    .by_name
                    .values()
                    .filter(|binding| binding.exposed)
                    .map(|binding| binding.descriptor.id.clone())
                    .collect(),
                revoked_tools: BTreeSet::new(),
                interactions: Vec::new(),
                answers: Vec::new(),
                output: Content::default(),
                structured: None,
                usage: Usage::default(),
                budget_usage: Usage::default(),
                child_runs: 0,
                outcome: None,
            };
            let mut checkpoint = Checkpoint::new(
                context.run_id,
                &request.session_id,
                "framework.react",
                1,
                serde_json::to_value(state).map_err(state_encoding)?,
            )
            .with_execution_scope(request.scope())
            .with_host_metadata(request);
            if let Some(snapshot) = initial_snapshot {
                checkpoint.conversation_span = Some(ConversationSpan {
                    key: snapshot.key,
                    generation: snapshot.generation,
                    start: snapshot.messages.len(),
                });
                checkpoint.transcript = snapshot.messages;
            } else if let Some(previous) = &previous {
                checkpoint.transcript = previous.transcript.clone();
            }
            checkpoint.transcript.push(
                Message::user(&request.input)
                    .with_annotations(request.annotations.clone())
                    .with_provenance(request.message_provenance.clone()),
            );

            checkpoint.history.messages.push(
                HistoryMessage::user(context.run_id, &request.input)
                    .with_annotations(request.annotations.clone())
                    .with_requested_skills(request.requested_skills.clone())
                    .with_provenance(request.message_provenance.clone()),
            );

            store.create(checkpoint, previous.map(|p| p.run_id)).await?
        };
        let state: ReactState =
            serde_json::from_value(checkpoint.state.clone()).map_err(state_encoding)?;
        if state.request.model_stage.trim().is_empty() {
            return Err(AgentError::InvalidConfiguration(
                "persisted ReAct definition differs from installed ReAct".into(),
            ));
        }
        state.request.limits.validate()?;
        if request.resume.is_some()
            && context.lineage.depth == 0
            && let Some(counter) = context.extensions.get::<ModelDispatchCounter>()
        {
            counter.restore(
                state.request.limits.clone(),
                state.budget_usage.clone(),
                state.steps,
                state.tool_calls,
                state.child_runs,
                !checkpoint.status.is_terminal() && matches!(&state.cursor, ReactCursor::Model),
            );
        }

        if checkpoint.status.is_terminal() {
            if let Some(resume) = &request.resume
                && resume
                    .response
                    .as_ref()
                    .is_some_and(|response| !state.answers.contains(response))
            {
                return Err(AgentError::InvalidConfiguration(
                    "terminal run cannot accept a new answer".into(),
                ));
            }
            if let Some(session) = &mut conversation {
                sync_transcript(
                    session.as_mut(),
                    &checkpoint.transcript,
                    state.conversation_generation.as_deref(),
                )
                .await?;
            }
            let result = match checkpoint.status {
                ExecutionStatus::Cancelled => Err(AgentError::Cancelled),
                ExecutionStatus::Failed => Err(AgentError::Run("persisted ReAct failed".into())),
                _ => Ok(RunResult {
                    output: state.output.text(),
                    content: state.output,
                    structured: state.structured,
                    interactions: state.interactions,
                    status: state.outcome.ok_or_else(|| {
                        AgentError::Run("terminal ReAct is missing its outcome".into())
                    })?,
                    usage: state.usage,
                    steps: state.steps,
                    tool_calls: state.tool_calls,
                    external_session: None,
                }),
            };
            if let Some(session) = &mut conversation {
                let finished = session.finish(ConversationOutcome::Stopped).await;
                return finish_with_cleanup(result, finished.map_err(Into::into));
            }
            return result;
        }
        let mut tools = match initial_tools.take() {
            Some(tools) => tools,
            None => crate::react::ToolSet::load(&services.tools, context).await?,
        };
        let lease = store
            .claim(checkpoint.run_id, RunId::new_v4(), self.lease_duration)
            .await?;
        let guard = RunLeaseGuard::new(
            store.clone(),
            lease.clone(),
            self.lease_duration,
            request.limits.cleanup_timeout,
            context.clone(),
        );
        let result = async {
            // Reload under the acquired fencing token: state may have advanced after
            // the optimistic read but before claim. Resume always acts on that state.
            checkpoint = store.load(checkpoint.run_id).await?;
            let mut state: ReactState =
                serde_json::from_value(checkpoint.state.clone()).map_err(state_encoding)?;
            if let ReactCursor::Tools {
                assistant,
                executions,
            } = &mut state.cursor
            {
                // The durable invocation remains authoritative, including when
                // loading a checkpoint written before ToolCall carried its ID.
                for execution in executions {
                    execution.call.invocation_id = execution.invocation_id;
                    if let Some(call) = assistant
                        .tool_calls
                        .iter_mut()
                        .find(|call| call.id == execution.call.id)
                    {
                        call.invocation_id = execution.invocation_id;
                    }
                }
            }
            // The optimistic snapshot can be replaced before we acquire the
            // lease. Never execute its successor without validating it again.
            checkpoint.validate()?;
            if checkpoint.run_id != context.run_id
                || checkpoint.session_id != request.session_id || checkpoint.execution_scope != request.scope()
                || checkpoint.loop_id != "framework.react"
                || checkpoint.loop_version != 1
                || state.request.session_id != checkpoint.session_id
                || checkpoint.status.is_terminal()
            {
                return Err(AgentError::InvalidConfiguration(
                    "acquired checkpoint does not match the installed ReAct and run".into(),
                ));
            }
            state.request.limits.validate()?;
            if !state.request.requested_skills.is_empty() && context.extensions.get::<ExplicitSkillSelectionSupport>().is_none() {
                return Err(AgentError::InvalidConfiguration("restored explicit skills require an installed skills runtime".into()));
            }
            if let Some(selected) = context.extensions.get::<RequestedSkills>() {
                selected.extend(&state.request.requested_skills)?;
                state.request.requested_skills = selected.names();
            }
            if request.resume.is_some() && context.lineage.depth == 0
                && let Some(counter) = context.extensions.get::<ModelDispatchCounter>()
            {
                counter.restore(state.request.limits.clone(), state.budget_usage.clone(), state.steps, state.tool_calls, state.child_runs, !checkpoint.status.is_terminal() && matches!(&state.cursor, ReactCursor::Model));
            }

            state.request.generation.validate()?;
            let mut execution_context = context.clone();
            execution_context.model = state.request.model.clone();
            execution_context.limits.max_steps = execution_context
                .limits
                .max_steps
                .min(state.request.limits.max_steps);
            execution_context.limits.max_total_tokens = execution_context
                .limits
                .max_total_tokens
                .min(state.request.limits.max_total_tokens);
            execution_context.token_ceiling = context
                .token_ceiling
                .min(state.request.limits.max_total_tokens);
            execution_context.max_child_runs = context
                .max_child_runs
                .min(state.request.limits.max_child_runs);
            execution_context.max_child_depth = context
                .max_child_depth
                .min(state.request.limits.max_delegation_depth);
            execution_context.deadline = context
                .deadline
                .min(tokio::time::Instant::now() + state.request.limits.run_timeout);
            let context = &execution_context;
            // An embedded ReAct shares its parent's restored ledger. Resetting
            // it here would erase reservations held by concurrent DAG siblings.
            if context.budget_journal.is_none() {
                context
                    .usage
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .usage = state.budget_usage.clone();
            }
            context
                .child_runs
                .fetch_max(state.child_runs, std::sync::atomic::Ordering::AcqRel);
            context.revoke_tools(&state.revoked_tools.iter().cloned().collect::<Vec<_>>())?;
            for binding in tools.by_name.values_mut() {
                binding.exposed = (tools.gateway
                    && (is_gateway_descriptor(&binding.descriptor)
                        || tools.native_ids.contains(&binding.descriptor.id)))
                    || state.visible_tools.contains(&binding.descriptor.id);
            }
            tools.refresh_gateway_search();
            let mut incoming = request.clone();
            if !state.interactions.is_empty() {
                let manager = services.interactions.as_ref().ok_or(AgentError::MissingService("InteractionStore"))?;
                manager.publish(&state.interactions, &state.cursor, context).await?;
                if let Some(response) = incoming.resume.as_ref().and_then(|r| r.response.as_ref())
                    && let Some(interaction) = state.interactions.iter().find(|i|i.id == response.interaction_id) {
                        interaction.validate_response(response, 0)?;
                        manager.0.answer_with_provenance(&context.session_id, response, incoming.message_provenance.as_ref()).await?;
                        incoming.resume.as_mut().unwrap().response = None;
                }
                // Apply committed answers only when their tool batch is complete.
                let unfinished = matches!(&state.cursor, ReactCursor::Tools { executions, .. } if executions.iter().any(|e| !matches!(e.state, ToolExecutionState::Finished(_))));
                let ids = state.interactions.iter()
                    .filter(|interaction| !matches!(interaction.kind, InteractionKind::Input) || !unfinished)
                    .map(|interaction| interaction.id).collect::<Vec<_>>();
                for record in manager.0.get_many(&context.session_id, &ids).await? {
                    if let Some(response) = record.response {
                        let mut answered = incoming.clone();
                        answered.message_provenance = record.response_provenance;
                        answered.resume = Some(ResumeRequest {run_id:context.run_id,response:Some(response)});
                        apply_resume(&answered, &mut state, &mut checkpoint, &tools)?;
                    }
                }
            }
            let resumed = apply_resume(&incoming, &mut state, &mut checkpoint, &tools);
            for binding in tools.by_name.values_mut() {
                binding.exposed = (tools.gateway
                    && (is_gateway_descriptor(&binding.descriptor)
                        || tools.native_ids.contains(&binding.descriptor.id)))
                    || state.visible_tools.contains(&binding.descriptor.id);
            }
            tools.refresh_gateway_search();
            context.revoke_tools(&state.revoked_tools.iter().cloned().collect::<Vec<_>>())?;
            match resumed {
                Err(error) => Err(error),
                Ok(()) => {
                    checkpoint.status = ExecutionStatus::Running;
                    if request.resume.is_some() {
                        for message in &mut checkpoint.history.messages {
                            if message.status==HistoryStatus::Running {message.status=HistoryStatus::Interrupted;}
                        }
                        for step in &mut checkpoint.history.steps {
                            if step.status==HistoryStatus::Running {step.status=HistoryStatus::Interrupted;step.finished_at_ms=Some(unix_time_ms());}
                        }
                    }
                    let journal = Arc::new(ReactBudgetJournal {
                        store: store.clone(),
                        lease: lease.clone(),
                        checkpoint: tokio::sync::Mutex::new(checkpoint.clone()),
                    });
                    let mut runtime = ModelExecutionRuntime {
                        journal,
                        store: store.as_ref(),
                        lease: &lease,
                        checkpoint,
                        state,
                        tools,
                        context,
                        services,
                        processed_results: Default::default(),
                        published_batch_snapshots: Default::default(),
                        conversation: conversation
                            .as_mut()
                            .map(|s| s.as_mut() as &mut dyn ConversationSession),
                    };
                    let result = match runtime.save(None).await {
                        Ok(()) => runtime.execute().await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = &result {
                        // Storage failures have an unknown write outcome. Never
                        // overwrite them with a guessed terminal transition.
                        if !error.contains_state_error() {
                            runtime.checkpoint.status =
                                if matches!(error.primary(), AgentError::Cancelled) {
                                    ExecutionStatus::Cancelled
                                } else {
                                    ExecutionStatus::Failed
                                };
                            let saved = runtime
                                .save(Some(AgentEvent::RunFailed(error.to_string())))
                                .await;
                            if saved.is_ok() {
                                if let ReactCursor::Tools {assistant,executions} = &runtime.state.cursor {
                                    for event in crate::tool_batches::interrupted_result_events(assistant,executions) {
                                        runtime.context.emit_output(event);
                                    }
                                }
                            }
                            return finish_with_cleanup(result, saved);
                        }
                    }
                    result
                }
            }
        }
        .await;
        let released = guard.close().await;
        let result = finish_with_cleanup(result, released.map_err(Into::into));
        if let Some(session) = &mut conversation {
            let outcome = match &result {
                Ok(r) if r.status == RunStatus::Completed => ConversationOutcome::Completed,
                Ok(r) if r.status == RunStatus::AwaitingInput => ConversationOutcome::AwaitingInput,
                Ok(_) => ConversationOutcome::Stopped,
                Err(e) if matches!(e.primary(), AgentError::Cancelled) => {
                    ConversationOutcome::Cancelled
                }
                Err(_) => ConversationOutcome::Failed,
            };
            let finished = session.finish(outcome).await;
            return finish_with_cleanup(result, finished.map_err(Into::into));
        }
        result
    }
}

async fn sync_transcript(
    session: &mut dyn ConversationSession,
    transcript: &[Message],
    generation: Option<&str>,
) -> Result<(), AgentError> {
    let snapshot = session.snapshot().await?;
    if generation.is_some_and(|g| g != snapshot.generation)
        || !transcript.starts_with(&snapshot.messages)
    {
        return Err(ConversationError::Conflict.into());
    }
    if snapshot.messages.len() < transcript.len() {
        session
            .append_at(snapshot.revision, &transcript[snapshot.messages.len()..])
            .await?;
    }
    Ok(())
}

fn state_encoding(_: serde_json::Error) -> AgentError {
    AgentError::State(StateError::Invalid(
        "ReAct state cannot be encoded or decoded".into(),
    ))
}

fn apply_resume(
    request: &RunRequest,
    state: &mut ReactState,
    checkpoint: &mut Checkpoint,
    tools: &crate::react::ToolSet,
) -> Result<(), AgentError> {
    let Some(resume) = &request.resume else {
        return Ok(());
    };
    if let Some(response) = &resume.response {
        if let Some(previous) = state
            .answers
            .iter()
            .find(|a| a.interaction_id == response.interaction_id)
        {
            if previous != response {
                return Err(AgentError::InvalidConfiguration(
                    "conflicting answer for resolved interaction".into(),
                ));
            }
        } else {
            let index = state
                .interactions
                .iter()
                .position(|i| i.id == response.interaction_id)
                .ok_or_else(|| {
                    AgentError::InvalidConfiguration(
                        "interaction is not pending on this run".into(),
                    )
                })?;
            let interaction = &state.interactions[index];
            // Expiry is enforced atomically by InteractionStore when accepting an answer.
            // A committed answer remains valid when recovery happens after expiry.
            interaction.validate_response(response, 0)?;
            match interaction.kind {
                InteractionKind::Input => {
                    if let ReactCursor::Tools {
                        assistant,
                        executions,
                    } = &state.cursor
                    {
                        if !executions.iter().any(|e| matches!(&e.state, ToolExecutionState::Finished(output) if matches!(&output.effect, ToolEffect::RequestInput(r) if r.id == interaction.id))) {
                            return Err(AgentError::InvalidConfiguration("input interaction has no matching tool result".into()));
                        }
                        let mut batch = vec![assistant.as_ref().clone()];
                        let mut completed = Vec::new();
                        for execution in executions {
                            let ToolExecutionState::Finished(output) = &execution.state else {
                                return Err(AgentError::InvalidConfiguration(
                                    "input answer requires a completed tool batch".into(),
                                ));
                            };
                            let mut output = output.clone();
                            match &output.effect {
                                ToolEffect::ExposeTools(ids) => {
                                    if ids.iter().all(|id| {
                                        tools.by_name.values().any(|b| &b.descriptor.id == id)
                                    }) {
                                        state.visible_tools.extend(ids.iter().cloned());
                                    } else {
                                        output = ToolOutput::error(
                                            "Tool exposure contains an unauthorized ID",
                                        );
                                    }
                                }
                                ToolEffect::SetExposedTools(ids) => {
                                    if ids.iter().all(|id| {
                                        tools.by_name.values().any(|b| &b.descriptor.id == id)
                                    }) {
                                        state.visible_tools = ids.iter().cloned().collect();
                                    } else {
                                        output = ToolOutput::error(
                                            "Tool exposure contains an unauthorized ID",
                                        );
                                    }
                                }
                                ToolEffect::RevokeTools(ids) => {
                                    state.revoked_tools.extend(ids.iter().cloned())
                                }
                                _ => {}
                            }
                            completed.push((execution, output));
                        }
                        batch.extend(crate::tool_batches::result_messages(&completed));
                        checkpoint.transcript.extend(batch);
                        state.cursor = ReactCursor::Model;
                    }
                    checkpoint.transcript.push(Message::user(Content(vec![ContentBlock::Json {
                        value: serde_json::json!({"interaction_id":response.interaction_id,"answer":response.value}),
                    }])).with_provenance(request.message_provenance.clone()));
                }
                InteractionKind::Approval { invocation_id } => {
                    let ReactCursor::Tools { executions, .. } = &mut state.cursor else {
                        return Err(AgentError::InvalidConfiguration(
                            "approval has no pending tool batch".into(),
                        ));
                    };
                    let execution = executions.iter_mut().find(|e| e.invocation_id == invocation_id && matches!(e.state,ToolExecutionState::WaitingApproval { interaction_id } if interaction_id == interaction.id)).ok_or_else(||AgentError::InvalidConfiguration("approval no longer matches pending invocation".into()))?;
                    execution.state = if response.value == serde_json::Value::Bool(true) {
                        ToolExecutionState::Approved
                    } else {
                        ToolExecutionState::Finished(ToolOutput::error(
                            "Tool denied by approval response",
                        ))
                    };
                }
            }
            if let Some(record) = checkpoint
                .history
                .interactions
                .iter_mut()
                .find(|i| i.id == response.interaction_id)
            {
                record.response = Some(response.value.clone());
                record.response_provenance = request.message_provenance.clone();
            }
            state.interactions.remove(index);
            state.answers.push(response.clone());
        }
    }
    Ok(())
}

struct ModelExecutionRuntime<'a> {
    // One authority for this lease, shared by every model/tool stage and any
    // retained child-accounting context. Phase-local snapshots can go stale.
    journal: Arc<ReactBudgetJournal>,
    store: &'a dyn StateStore,
    lease: &'a ExecutionLease,
    checkpoint: Checkpoint,
    state: ReactState,
    tools: crate::react::ToolSet,
    context: &'a RunContext,
    services: &'a AgentServices,
    // A failed commit must not invoke host post-processing a second time during
    // terminal cleanup. Durable history takes over this guard after a commit.
    processed_results: BTreeSet<RunId>,
    published_batch_snapshots:
        std::collections::BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
    conversation: Option<&'a mut dyn ConversationSession>,
}
impl ModelExecutionRuntime<'_> {
    fn has_started_tools(&self) -> bool {
        matches!(&self.state.cursor,ReactCursor::Tools { executions,.. } if executions.iter().any(|e|matches!(e.state,ToolExecutionState::Started)))
    }
    fn result(&self, status: RunStatus) -> RunResult {
        RunResult {
            output: self.state.output.text(),
            content: self.state.output.clone(),
            structured: self.state.structured.clone(),
            interactions: self.state.interactions.clone(),
            status,
            usage: self.state.usage.clone(),
            steps: self.state.steps,
            tool_calls: self.state.tool_calls,
            external_session: None,
        }
    }
    fn update_history(&mut self) {
        let now = unix_time_ms();
        if let ReactCursor::Tools { executions, .. } = &self.state.cursor {
            crate::tool_batches::update_batch_history(
                executions,
                &mut self.checkpoint.history,
                now,
                self.checkpoint.status.is_terminal(),
            );
            for execution in executions {
                let Some(record) = self
                    .checkpoint
                    .history
                    .tool_invocations
                    .iter_mut()
                    .find(|t| t.id == execution.invocation_id)
                else {
                    continue;
                };
                record.tool_id = execution.tool_id.clone();
                record.status = match &execution.state {
                    ToolExecutionState::Planned => HistoryStatus::Planned,
                    ToolExecutionState::Approved => HistoryStatus::Approved,
                    ToolExecutionState::WaitingApproval { .. } => HistoryStatus::Waiting,
                    ToolExecutionState::Started => {
                        if self.checkpoint.status.is_terminal() {
                            HistoryStatus::Unknown
                        } else {
                            HistoryStatus::Running
                        }
                    }
                    ToolExecutionState::Finished(output) => {
                        record.content = history_content(&output.content);
                        record.structured = output.structured.clone();
                        record.is_error = Some(output.is_error);
                        record.result_reference = output.result_reference.as_ref().map(|r| {
                            HistoryContentBlock::Resource {
                                uri: r.uri.clone(),
                                mime_type: r.mime_type.clone(),
                                name: r.name.clone(),
                                size_bytes: r.size_bytes,
                            }
                        });
                        record.finished_at_ms.get_or_insert(now);
                        if output.is_error {
                            HistoryStatus::Failed
                        } else {
                            HistoryStatus::Completed
                        }
                    }
                };
                if matches!(execution.state, ToolExecutionState::Started) {
                    record.started_at_ms.get_or_insert(now);
                }
            }
        }
        for request in &self.state.interactions {
            if !self
                .checkpoint
                .history
                .interactions
                .iter()
                .any(|i| i.id == request.id)
            {
                self.checkpoint
                    .history
                    .interactions
                    .push(HistoryInteraction {
                        id: request.id,
                        run_id: self.context.run_id,
                        invocation_id: match request.kind {
                            InteractionKind::Approval { invocation_id } => Some(invocation_id),
                            InteractionKind::Input => None,
                        },
                        prompt: history_content(&request.prompt),
                        response_schema: request.response_schema.clone(),
                        presentation: request.presentation.clone(),
                        response: None,
                        response_provenance: None,
                        expires_at_ms: request.expires_at_unix_ms,
                    });
            }
        }
        for response in &self.state.answers {
            if let Some(record) = self
                .checkpoint
                .history
                .interactions
                .iter_mut()
                .find(|i| i.id == response.interaction_id)
            {
                record.response = Some(response.value.clone());
            }
        }
    }
    async fn process_finished_history_tools(&mut self) -> Result<(), AgentError> {
        let ReactCursor::Tools { executions, .. } = &self.state.cursor else {
            return Ok(());
        };
        let pending: Vec<_> = executions
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(e.state, ToolExecutionState::Finished(_))
                    && !self.processed_results.contains(&e.invocation_id)
                    && self
                        .checkpoint
                        .history
                        .tool_invocations
                        .iter()
                        .find(|t| t.id == e.invocation_id)
                        .is_none_or(|t| t.is_error.is_none())
            })
            .map(|(i, e)| (i, e.clone()))
            .collect();
        for (index, execution) in pending {
            let ToolExecutionState::Finished(mut output) = execution.state.clone() else {
                continue;
            };
            let processed = std::panic::AssertUnwindSafe(async {
                // Resolve native effects before publishing/auditing a final result.
                // A rejected exposure must never be persisted as a success.
                output = self.apply_tool_effect(output)?;
                let applied_effect = output.effect.clone();
                if let Some(processor) = self.context.extensions.get::<ToolResultProcessorService>()
                {
                    let was_error = output.is_error;
                    let item_context =
                        crate::tool_batches::invocation_context(self.context, &execution);
                    output = processor
                        .0
                        .process(
                            &ToolInvocationContext {
                                invocation_id: &execution.invocation_id.to_string(),
                                call: &execution.call,
                                descriptor: &execution.descriptor,
                                run: &item_context,
                            },
                            output,
                        )
                        .await?;
                    if was_error && !output.is_error {
                        return Err(AgentError::Run(
                            "tool result processor converted a failure to success".into(),
                        ));
                    }
                }
                // Host processors may supply an exposure effect of their own.
                if output.effect != applied_effect {
                    output = self.apply_tool_effect(output)?;
                }
                Ok::<_, AgentError>(output)
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(AgentError::Run("tool result processor panicked".into())));
            let output = match processed {
                Ok(output) => output,
                Err(error) => {
                    // The call already returned. Do not expose unprocessed data
                    // (a host processor may redact it), or claim it never ran.
                    // Report a failed result to the model without replaying the
                    // call or aborting unrelated tools in the same batch.
                    ToolOutput::error(format!(
                        "Tool result processing failed: {error}. Execution effects may already exist. Verify them before retrying; the unprocessed result is unavailable."
                    ))
                }
            };
            self.execution_mut(index).state = ToolExecutionState::Finished(output);
            self.processed_results.insert(execution.invocation_id);
        }
        Ok(())
    }
    fn apply_tool_effect(&mut self, mut output: ToolOutput) -> Result<ToolOutput, AgentError> {
        match &output.effect {
            ToolEffect::ExposeTools(ids) => {
                if let Err(error) = self.tools.apply_exposure(ids) {
                    output = ToolOutput::error(error);
                } else {
                    self.state.visible_tools.extend(ids.iter().cloned());
                }
            }
            ToolEffect::SetExposedTools(ids) => {
                if let Err(error) = self.tools.set_exposure(ids) {
                    output = ToolOutput::error(error);
                } else {
                    self.state.visible_tools = ids.iter().cloned().collect();
                }
            }
            ToolEffect::RevokeTools(ids) => self.context.revoke_tools(ids)?,
            ToolEffect::RequestInput(_) | ToolEffect::AwaitInput | ToolEffect::Continue => {}
        }
        Ok(output)
    }
    async fn save(&mut self, mut event: Option<AgentEvent>) -> Result<(), AgentError> {
        self.process_finished_history_tools().await?;
        // Commit recovery context together with terminal history. A later user
        // turn must see confirmed effects and unknown outcomes, not repeat the
        // original plan. Keep the cursor for audit; terminal runs never dispatch it.
        if self.checkpoint.status.is_terminal()
            && let ReactCursor::Tools {
                assistant,
                executions,
            } = &self.state.cursor
            && !self.checkpoint.transcript.contains(assistant.as_ref())
        {
            self.checkpoint.transcript.push(assistant.as_ref().clone());
            self.checkpoint
                .transcript
                .extend(crate::tool_batches::interrupted_result_messages(
                    assistant, executions,
                ));
        }
        if let Some(AgentEvent::ToolFinished {
            call_id, is_error, ..
        }) = &mut event
            && let ReactCursor::Tools { executions, .. } = &self.state.cursor
            && let Some(ToolExecution {
                state: ToolExecutionState::Finished(output),
                ..
            }) = executions
                .iter()
                .find(|execution| execution.call.id == *call_id)
        {
            *is_error = output.is_error;
        }
        self.update_history();
        if self.checkpoint.status.is_terminal() {
            self.state.interactions.clear();
        }
        self.state.revoked_tools = self
            .context
            .revoked_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Serialize ReAct transitions with budget/history commits from current
        // and earlier stages. Only this lease's shared head supplies the revision;
        // an external executor still must pass the store's CAS and fencing checks.
        let journal = self.journal.clone();
        let mut current = journal.checkpoint.lock().await;
        self.checkpoint.revision = current.revision;
        self.state.budget_usage = self
            .context
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .including_reservations();
        self.state.child_runs = self
            .context
            .child_runs
            .load(std::sync::atomic::Ordering::Acquire);
        self.checkpoint.state = serde_json::to_value(&self.state).map_err(state_encoding)?;
        if let Some(event) = &event {
            self.checkpoint.events.push(EventEnvelope {
                scope: EventScope::Journal,
                schema_version: 1,
                run_id: self.checkpoint.run_id,
                session_id: self.checkpoint.session_id.clone(),
                sequence: self.checkpoint.events.last().map_or(1, |e| e.sequence + 1),
                timestamp_unix_ms: unix_time_ms(),
                event: event.clone(),
            });
        }
        self.checkpoint = self
            .store
            .commit(
                self.lease,
                self.checkpoint.revision,
                self.checkpoint.clone(),
            )
            .await?;
        *current = self.checkpoint.clone();
        drop(current);
        if self.checkpoint.status.is_terminal() {
            if let Some(manager) = &self.services.interactions {
                // Also close independently configured/remote interaction stores.
                manager
                    .0
                    .close_run(&self.checkpoint.session_id, self.checkpoint.run_id)
                    .await?;
            }
        } else if !self.state.interactions.is_empty() {
            self.services
                .interactions
                .as_ref()
                .ok_or(AgentError::MissingService("InteractionStore"))?
                .publish(&self.state.interactions, &self.state.cursor, self.context)
                .await?;
        }
        self.sync_conversation().await?;
        // Publish only committed states. Resumes get a fresh snapshot, while
        // repeated saves of the same state produce no duplicate progress frame.
        if !self.checkpoint.status.is_terminal()
            && let ReactCursor::Tools {
                assistant,
                executions,
            } = &self.state.cursor
        {
            for event in crate::tool_batches::batch_snapshot_events(
                assistant,
                executions,
                &self.checkpoint.history,
            ) {
                if let OutputEvent::ActivitySnapshot { id, content, .. } = &event {
                    if self.published_batch_snapshots.get(id) == Some(content) {
                        continue;
                    }
                    self.published_batch_snapshots
                        .insert(id.clone(), content.clone());
                }
                self.context.emit_output(event);
            }
        }
        if let Some(event) = event {
            // Agent owns the sole live terminal event; checkpoints keep a separate journal.
            if !matches!(
                event,
                AgentEvent::RunFailed(_)
                    | AgentEvent::RunCancelled
                    | AgentEvent::RunFinished { .. }
            ) {
                self.context.emit(event).await;
            }
        }
        Ok(())
    }
    async fn sync_conversation(&mut self) -> Result<(), AgentError> {
        if let Some(session) = &mut self.conversation {
            sync_transcript(
                *session,
                &self.checkpoint.transcript,
                self.state.conversation_generation.as_deref(),
            )
            .await?;
        }
        Ok(())
    }
    async fn record_steering(&mut self, pending: Vec<LiveInput>) -> Result<(), AgentError> {
        for input in pending {
            if !input.requested_skills.is_empty()
                && self
                    .context
                    .extensions
                    .get::<ExplicitSkillSelectionSupport>()
                    .is_none()
            {
                return Err(AgentError::InvalidConfiguration(
                    "live explicit skills require an installed skills runtime".into(),
                ));
            }
            if let Some(selected) = self.context.extensions.get::<RequestedSkills>() {
                selected.extend(&input.requested_skills)?;
                self.state.request.requested_skills = selected.names();
            }
            validate_annotations(&input.annotations).map_err(AgentError::InvalidConfiguration)?;
            let annotations = input.annotations;
            let requested_skills = input.requested_skills;
            let provenance = input
                .provenance
                .or_else(|| self.context.message_provenance());
            let input = input.content;
            self.checkpoint.history.messages.push(
                HistoryMessage::user(self.context.run_id, &input)
                    .with_annotations(annotations.clone())
                    .with_requested_skills(requested_skills)
                    .with_provenance(provenance.clone()),
            );
            self.checkpoint.transcript.push(
                Message::user(input.clone())
                    .with_annotations(annotations)
                    .with_provenance(provenance),
            );
            // Persist the message and its journal event together before
            // publishing the live event, including at the final reply boundary.
            self.save(Some(AgentEvent::Steered(input.protocol_text())))
                .await?;
        }
        Ok(())
    }
    async fn stop(
        &mut self,
        status: RunStatus,
        execution: ExecutionStatus,
    ) -> Result<RunResult, AgentError> {
        if status == RunStatus::AwaitingInput {
            self.state.output = self
                .state
                .interactions
                .first()
                .map(|r| r.prompt.clone())
                .unwrap_or_default();
            self.state.structured = None;
        }
        self.checkpoint.status = execution;
        self.state.outcome = Some(status.clone());
        self.save(None).await?;
        if let ReactCursor::Tools {
            assistant,
            executions,
        } = &self.state.cursor
        {
            let completed: Vec<_> = executions
                .iter()
                .filter_map(|execution| {
                    let ToolExecutionState::Finished(output) = &execution.state else {
                        return None;
                    };
                    if let Some(item) = &execution.batch_item {
                        if executions.iter().any(|e| {
                            e.batch_item.as_ref().is_some_and(|i| {
                                i.parent_invocation_id == item.parent_invocation_id
                            }) && !matches!(e.state, ToolExecutionState::Finished(_))
                        }) {
                            return None;
                        }
                    }
                    Some((execution, output.clone()))
                })
                .collect();
            for event in crate::tool_batches::result_events(assistant, &completed) {
                self.context.emit_output(event);
            }
        }
        Ok(self.result(status))
    }
    async fn execute(&mut self) -> Result<RunResult, AgentError> {
        loop {
            self.context.check_cancelled()?;
            if self.has_started_tools() {
                return Err(AgentError::Run(
                    "tool execution was interrupted before its result was saved".into(),
                ));
            }
            if !self.state.interactions.is_empty() {
                return self
                    .stop(RunStatus::AwaitingInput, ExecutionStatus::Waiting)
                    .await;
            }
            match &self.state.cursor {
                ReactCursor::Model => {
                    if self.state.pending_model_response.is_none()
                        && (RunLimits::count_exhausted(
                            self.state.steps,
                            self.state.request.limits.max_steps,
                        ) || !admit_native_step(
                            self.context,
                            self.state.request.limits.max_steps,
                        ))
                    {
                        return self
                            .stop(RunStatus::StepLimit, ExecutionStatus::Completed)
                            .await;
                    }
                    let mut steering: Vec<LiveInput> = self.context.take_steering_inputs();
                    if let Some(source) = self.context.extensions.get::<LiveInputSourceService>() {
                        steering.extend(source.0.drain_inputs(self.context, false).await?);
                    }
                    self.record_steering(steering).await?;
                    let instructions = Vec::new();
                    let descriptors: Vec<_> = self
                        .tools
                        .descriptors()
                        .into_iter()
                        .filter(|tool| !self.context.is_tool_revoked(&tool.id))
                        .collect();
                    let mut memory = if let Some(session) = &mut self.conversation {
                        TranscriptProjection::from_snapshot(session.snapshot().await?)
                    } else {
                        TranscriptProjection::new(self.checkpoint.transcript.clone()).with_key(
                            ConversationKey::new(&self.context.session_id).branch(
                                serde_json::to_string(&(
                                    self.context.run_id,
                                    "framework.react",
                                    &self.state.request.model_stage,
                                ))
                                .expect("ReAct identity is serializable"),
                            ),
                        )
                    };
                    let journal = self.journal.clone();
                    let mut model_context = self.context.clone();
                    model_context.budget_journal = Some(journal.clone());
                    model_context
                        .extensions
                        .insert(HistoryCommitterService(journal.clone()));
                    let response = if let Some(response) = self.state.pending_model_response.take()
                    {
                        Ok(response)
                    } else {
                        self.services
                            .models
                            .as_ref()
                            .ok_or(AgentError::MissingService("ChatModel"))?
                            .invoke(
                                ModelCall {
                                    stage: &self.state.request.model_stage,
                                    step: self.state.steps + 1,
                                    instructions: &instructions,
                                    tools: &descriptors,
                                    limits: &self.state.request.limits,
                                    options: &self.state.request.generation,
                                },
                                &mut memory,
                                &model_context,
                            )
                            .await
                    };
                    self.checkpoint = journal.checkpoint.lock().await.clone();
                    let response = response?;
                    // Maintenance assembles and persists context through the same model
                    // service, but has no reply message or further ReAct stage.
                    if self
                        .context
                        .extensions
                        .get::<ContextCompactionRequest>()
                        .is_some()
                    {
                        return self
                            .stop(RunStatus::Completed, ExecutionStatus::Completed)
                            .await;
                    }
                    self.state.steps += 1;
                    self.state.usage.add(&response.usage);
                    self.state.output = response.content.clone();
                    self.state.structured = response.structured.clone();
                    let disposition = response.disposition;
                    let mut assistant_messages = response.assistant_messages();
                    let output_group_id = (!response.output_items.is_empty()).then(|| {
                        self.checkpoint
                            .history
                            .steps
                            .last()
                            .map(|step| step.id.clone())
                            .unwrap_or_else(|| RunId::new_v4().to_string())
                    });
                    for message in &mut assistant_messages {
                        message.provenance = self.context.assistant_provenance();
                        message.output_group_id = output_group_id.clone();
                    }
                    if response.tool_calls.is_empty() {
                        self.checkpoint.transcript.extend(assistant_messages);
                        if disposition == Some(ModelDisposition::Continue) {
                            if self.state.phase_continuations
                                >= self.state.request.limits.max_phase_continuations
                            {
                                self.save(None).await?;
                                return Err(AgentError::Run(
                                    "model returned only commentary without a final answer".into(),
                                ));
                            }
                            self.state.phase_continuations += 1;
                            self.save(None).await?;
                            continue;
                        }
                        if self.state.output.is_empty() {
                            self.save(None).await?;
                            return Err(AgentError::Run("model returned empty content".into()));
                        }

                        let mut steering: Vec<LiveInput> = self.context.take_steering_inputs();
                        if let Some(source) =
                            self.context.extensions.get::<LiveInputSourceService>()
                        {
                            steering.extend(
                                source
                                    .0
                                    .drain_inputs(self.context, steering.is_empty())
                                    .await?,
                            );
                        }
                        if steering.is_empty() {
                            steering = self.context.pending_inputs_or_seal();
                        }
                        if steering.is_empty() {
                            return self
                                .stop(RunStatus::Completed, ExecutionStatus::Completed)
                                .await;
                        }
                        self.record_steering(steering).await?;
                    } else {
                        // The execution cursor needs the whole call batch; the
                        // ordered provider messages are persisted separately.
                        let mut assistant = Message::assistant(response.content.clone());
                        assistant.tool_calls = response.tool_calls.clone();
                        self.state.pending_assistant_messages = assistant_messages;
                        let expanded =
                            crate::tool_batches::expand_calls(&assistant.tool_calls, &self.tools)?;
                        let remaining = if self.state.request.limits.max_tool_calls
                            == RunLimits::UNLIMITED_COUNT
                        {
                            expanded.len()
                        } else {
                            self.state
                                .request
                                .limits
                                .max_tool_calls
                                .saturating_sub(self.state.tool_calls)
                        };
                        let overflow = expanded.len() > remaining;
                        if overflow
                            && (remaining == 0
                                || self.state.request.limits.tool_batch_budget_policy
                                    == ToolBatchBudgetPolicy::RejectBatch)
                        {
                            return self
                                .stop(RunStatus::ToolCallLimit, ExecutionStatus::Completed)
                                .await;
                        }
                        let admitted = expanded.len().min(remaining);
                        if !admit_native_tools(
                            self.context,
                            self.state.request.limits.max_tool_calls,
                            admitted,
                        ) {
                            return self
                                .stop(RunStatus::ToolCallLimit, ExecutionStatus::Completed)
                                .await;
                        }
                        self.state.tool_batch_limit_reached = overflow;
                        let mut executions = Vec::new();
                        for (index, (call, batch_item, batch_error)) in expanded.iter().enumerate()
                        {
                            let binding = self.tools.by_name.get(&call.call_name).filter(|b| {
                                descriptors.iter().any(|d| d.id == b.descriptor.id)
                                    || (self.state.request.limits.allow_unexposed_tools
                                        && !self.context.is_tool_revoked(&b.descriptor.id))
                            });
                            // Gateway unwrap: a `tool_execute` envelope is
                            // rewritten to the inner tool's call so schema
                            // validation, policy, middleware, audit and
                            // dispatch all see the real tool. The assistant
                            // transcript keeps the provider-faithful envelope;
                            // the provider call id and invocation identity are
                            // preserved for result pairing.
                            let via_gateway = binding
                                .as_ref()
                                .is_some_and(|b| b.descriptor.id == TOOL_EXECUTE_ID);
                            let (descriptor, state, execution_call) = if via_gateway {
                                match GatewayEnvelope::parse(&call.arguments) {
                                    Ok(envelope) => {
                                        match self.tools.by_name.get(&envelope.tool).filter(|b| {
                                            (b.exposed
                                                || self.state.request.limits.allow_unexposed_tools)
                                                && !self.context.is_tool_revoked(&b.descriptor.id)
                                        }) {
                                            Some(inner) => (
                                                inner.descriptor.clone(),
                                                ToolExecutionState::Planned,
                                                envelope.unwrap_call(call),
                                            ),
                                            None => (
                                                ToolDescriptor::new(
                                                    "unavailable",
                                                    &envelope.tool,
                                                    "Unavailable tool",
                                                    serde_json::json!({}),
                                                ),
                                                ToolExecutionState::Finished(ToolOutput::error(
                                                    "Tool is not exposed for this stage",
                                                )),
                                                // Keep the inner name so rejection
                                                // diagnostics address the tool the
                                                // model actually asked for.
                                                envelope.unwrap_call(call),
                                            ),
                                        }
                                    }
                                    Err(message) => (
                                        ToolDescriptor::new(
                                            "unavailable",
                                            &call.call_name,
                                            "Unavailable tool",
                                            serde_json::json!({}),
                                        ),
                                        ToolExecutionState::Finished(ToolOutput::error(format!(
                                            "Invalid tool_execute arguments: {message}"
                                        ))),
                                        call.clone(),
                                    ),
                                }
                            } else {
                                match binding {
                                    Some(binding) => (
                                        binding.descriptor.clone(),
                                        ToolExecutionState::Planned,
                                        call.clone(),
                                    ),
                                    None => (
                                        ToolDescriptor::new(
                                            "unavailable",
                                            &call.call_name,
                                            "Unavailable tool",
                                            serde_json::json!({}),
                                        ),
                                        ToolExecutionState::Finished(ToolOutput::error(
                                            "Tool is not exposed for this stage",
                                        )),
                                        call.clone(),
                                    ),
                                }
                            };
                            let state = if let Some(error) = batch_error {
                                ToolExecutionState::Finished(ToolOutput::error(error))
                            } else if index >= admitted {
                                ToolExecutionState::Finished(ToolOutput::error(
                                    "Tool not executed: tool-call budget exhausted",
                                ))
                            } else {
                                state
                            };
                            executions.push(ToolExecution {
                                invocation_id: call.invocation_id,
                                tool_id: descriptor.id.clone(),
                                descriptor,
                                call: execution_call,
                                state,
                                via_gateway,
                                batch_item: batch_item.clone(),
                                preparation: None,
                            });
                        }
                        self.state.tool_calls += admitted;
                        self.state.cursor = ReactCursor::Tools {
                            assistant: Box::new(assistant),
                            executions,
                        };
                        self.save(None).await?;
                    }
                }
                ReactCursor::Tools { .. } => {
                    if let Some(result) = self.execute_tools().await? {
                        return Ok(result);
                    }
                }
            }
        }
    }
    async fn execute_tools(&mut self) -> Result<Option<RunResult>, AgentError> {
        if let ReactCursor::Tools { executions, .. } = &self.state.cursor {
            for execution in executions {
                if let ToolExecutionState::Finished(output) = &execution.state
                    && let ToolEffect::RevokeTools(ids) = &output.effect
                {
                    self.context.revoke_tools(ids)?;
                }
            }
        }
        let ReactCursor::Tools { executions, .. } = &self.state.cursor else {
            unreachable!()
        };
        let count = executions.len();
        // Freeze every resource observation before any sibling can mutate state.
        for index in 0..count {
            let execution = self.execution(index).clone();
            if !matches!(
                execution.state,
                ToolExecutionState::Planned | ToolExecutionState::Approved
            ) || execution.preparation.is_some()
            {
                continue;
            }
            if let Some(binding) = self.tools.by_name.get(&execution.call.call_name).cloned() {
                let context = crate::tool_batches::invocation_context(self.context, &execution);
                let result = tokio::select! {
                    biased;
                    _ = self.context.cancelled() => return Err(AgentError::Cancelled),
                    result = tokio::time::timeout_at(self.context.deadline, binding.executor.prepare(&execution.call.arguments, &context)) => result.map_err(|_| AgentError::Run("tool preparation deadline exceeded".into()))?,
                };
                match result {
                    Ok(preparation) => self.execution_mut(index).preparation = preparation,
                    Err(error) => {
                        self.execution_mut(index).state =
                            ToolExecutionState::Finished(ToolOutput::error(error.to_string()))
                    }
                }
            }
        }
        let mut overlapping = std::collections::BTreeSet::new();
        for left in 0..count {
            for right in left + 1..count {
                let (Some(a), Some(b)) = (
                    &self.execution(left).preparation,
                    &self.execution(right).preparation,
                ) else {
                    continue;
                };
                if a.conflicts_with(b) {
                    overlapping.insert(left);
                    overlapping.insert(right);
                }
            }
        }
        for index in overlapping {
            if matches!(
                self.execution(index).state,
                ToolExecutionState::Planned | ToolExecutionState::Approved
            ) {
                self.execution_mut(index).state = ToolExecutionState::Finished(ToolOutput::error(
                    "notApplied: overlapping writers in one model response; combine related changes into one patch",
                ));
            }
        }
        // Evaluate the entire batch before effects. An approval-gated sibling
        // cannot be overtaken by a supposedly parallel-safe tool.
        for index in 0..count {
            let execution = self.execution(index).clone();
            if matches!(
                execution.state,
                ToolExecutionState::Finished(_) | ToolExecutionState::WaitingApproval { .. }
            ) {
                continue;
            }
            let binding = self
                .tools
                .by_name
                .get(&execution.call.call_name)
                .filter(|b| {
                    (b.exposed || self.state.request.limits.allow_unexposed_tools)
                        && !self.context.is_tool_revoked(&b.descriptor.id)
                        && b.descriptor == execution.descriptor
                });
            let Some(binding) = binding else {
                self.execution_mut(index).state = ToolExecutionState::Finished(ToolOutput::error(
                    "Tool not executed: its contract is no longer available. Discover the current tools before continuing.",
                ));
                continue;
            };
            let validation = compile_schema(&binding.descriptor.input_schema)
                .map(|schema| schema.is_valid(&execution.call.arguments));
            if !matches!(validation, Ok(true)) {
                let message = match validation {
                    Err(error) => format!("Tool not executed: invalid input schema: {error}"),
                    _ => "Tool arguments do not match input schema".into(),
                };
                self.execution_mut(index).state =
                    ToolExecutionState::Finished(ToolOutput::error(message));
                continue;
            }
            let invocation_id = execution.invocation_id.to_string();
            let item_context = crate::tool_batches::invocation_context(self.context, &execution);
            let invocation = ToolInvocationContext {
                invocation_id: &invocation_id,
                call: &execution.call,
                descriptor: &execution.descriptor,
                run: &item_context,
            };
            let approved = matches!(&execution.state, ToolExecutionState::Approved);
            let mut approval = None;
            let mut denied = None;
            for policy in &self.services.tool_policy {
                let decision = std::panic::AssertUnwindSafe(policy.evaluate(&invocation))
                    .catch_unwind()
                    .await;
                match decision {
                    Ok(Ok(ToolPolicyDecision::Allow)) => {}
                    Ok(Ok(ToolPolicyDecision::Deny(reason))) => {
                        denied = Some(reason);
                        break;
                    }
                    Ok(Ok(ToolPolicyDecision::RequireApproval(prompt))) if !approved => {
                        if approval.is_none() {
                            approval = Some(prompt);
                        }
                    }
                    Ok(Ok(ToolPolicyDecision::RequireApproval(_))) => {}
                    Ok(Err(error)) => {
                        denied = Some(format!(
                            "Tool not executed: policy evaluation failed: {error}"
                        ));
                        break;
                    }
                    Err(_) => {
                        denied = Some("Tool not executed: policy evaluation panicked".into());
                        break;
                    }
                }
            }
            if let Some(reason) = denied {
                self.execution_mut(index).state =
                    ToolExecutionState::Finished(ToolOutput::error(reason));
                continue;
            }
            if let Some(prompt) = approval {
                let mut interaction = InteractionRequest::approval(execution.invocation_id, prompt);
                interaction.presentation = Some(
                    serde_json::to_value(ToolApprovalDetails {
                        tool: execution.call.call_name.clone(),
                        parameters: execution.call.arguments.clone(),
                        batch: execution.batch_item.clone(),
                    })
                    .expect("approval details serialize"),
                );
                self.execution_mut(index).state = ToolExecutionState::WaitingApproval {
                    interaction_id: interaction.id,
                };
                self.state.interactions.push(interaction);
            }
        }
        self.save(None).await?;
        if !self.state.interactions.is_empty() {
            return self
                .stop(RunStatus::AwaitingInput, ExecutionStatus::Waiting)
                .await
                .map(Some);
        }
        if !self.execute_parallel(count).await? {
            for index in 0..count {
                if matches!(self.execution(index).state, ToolExecutionState::Finished(_)) {
                    continue;
                }
                self.context.check_cancelled()?;
                let execution = self.execution(index).clone();
                self.execution_mut(index).state = ToolExecutionState::Started;
                self.save(Some(AgentEvent::ToolStarted {
                    tool_id: execution.tool_id.clone(),
                    call_id: execution.call.id.clone(),
                }))
                .await?;
                let journal = self.journal.clone();
                let mut context = crate::tool_batches::invocation_context(self.context, &execution);
                context.budget_journal = Some(journal.clone());
                let result = self
                    .services
                    .tool_invoker
                    .invoke(
                        &execution.invocation_id.to_string(),
                        &execution.call,
                        &self.tools.by_name[&execution.call.call_name],
                        &context,
                        self.state.request.limits.tool_timeout,
                    )
                    .await;
                self.checkpoint = journal.checkpoint.lock().await.clone();
                self.finish_tool_result(index, result).await?;
            }
        }
        // Known results are durable before cancellation can terminate the run.
        self.context.check_cancelled()?;
        if self.has_started_tools() {
            return Err(AgentError::Run(
                "tool execution ended without a confirmed result".into(),
            ));
        }
        self.complete_tool_batch().await?;
        if self.state.tool_batch_limit_reached {
            return self
                .stop(RunStatus::ToolCallLimit, ExecutionStatus::Completed)
                .await
                .map(Some);
        }
        if !self.state.interactions.is_empty() {
            return self
                .stop(RunStatus::AwaitingInput, ExecutionStatus::Waiting)
                .await
                .map(Some);
        }
        Ok(None)
    }
    async fn finish_tool_result(
        &mut self,
        index: usize,
        result: Result<ToolOutput, ToolError>,
    ) -> Result<(), AgentError> {
        let execution = self.execution(index).clone();
        let mut output = match result {
            Ok(output) => output,
            Err(error @ ToolError::Indeterminate(_)) => {
                // Finish and persist this invocation so recovery never silently
                // dispatches it again. Uncertainty is a tool result, not a reason
                // to stop the model from inspecting state or choosing another tool.
                let mut output = ToolOutput::error(format!(
                    "{error}. The operation may already have taken effect. Do not repeat it automatically. Inspect fresh external state before deciding whether to act again."
                ));
                output.structured = Some(serde_json::json!({
                    "kind": "toolExecutionError", "code": "TOOL_OUTCOME_UNCERTAIN",
                    "outcomeUncertain": true, "tool": execution.call.call_name,
                    "message": error.to_string(), "nextStep": "inspect_state",
                }));
                output
            }
            Err(error) => ToolOutput::error(error.to_string()),
        };
        if output.effect == ToolEffect::AwaitInput {
            output = match InteractionRequest::input(
                &output.content,
                serde_json::json!({"type":"string"}),
            ) {
                Ok(interaction) => output.request_input(interaction),
                Err(error) => {
                    ToolOutput::error(format!("Tool returned an invalid input request: {error}"))
                }
            };
        }
        if let ToolEffect::RequestInput(interaction) = &output.effect {
            let invalid_identity = !matches!(interaction.kind, InteractionKind::Input)
                || interaction.id.is_nil()
                || self
                    .state
                    .interactions
                    .iter()
                    .any(|i| i.id == interaction.id)
                || self
                    .state
                    .answers
                    .iter()
                    .any(|a| a.interaction_id == interaction.id);
            let schema = compile_response_schema(&interaction.response_schema);
            if invalid_identity {
                output = ToolOutput::error("Tool returned invalid interaction identity/kind");
            } else if let Err(error) = schema {
                output = ToolOutput::error(format!(
                    "Tool returned an invalid interaction schema: {error}"
                ));
            } else {
                self.state.interactions.push(interaction.as_ref().clone());
                // An answer may change the plan. In-flight siblings must still be
                // collected; only calls not dispatched yet can be skipped.
                if let ReactCursor::Tools { executions, .. } = &mut self.state.cursor {
                    for (other, pending) in executions.iter_mut().enumerate() {
                        if other != index
                            && matches!(
                                pending.state,
                                ToolExecutionState::Planned | ToolExecutionState::Approved
                            )
                        {
                            pending.state = ToolExecutionState::Finished(ToolOutput::error(
                                "Tool not executed: awaiting input; replan after the answer",
                            ));
                        }
                    }
                }
            }
        }
        let is_error = output.is_error;
        self.execution_mut(index).state = ToolExecutionState::Finished(output);
        self.save(Some(AgentEvent::ToolFinished {
            tool_id: execution.tool_id,
            call_id: execution.call.id,
            is_error,
        }))
        .await
    }
    async fn execute_parallel(&mut self, count: usize) -> Result<bool, AgentError> {
        let indices: Vec<_> = (0..count)
            .filter(|&i| !matches!(self.execution(i).state, ToolExecutionState::Finished(_)))
            .collect();
        let limit = self.state.request.limits.max_parallel_tools;
        if limit < 2
            || indices.len() < 2
            || indices
                .iter()
                .any(|&i| !self.tools.by_name[&self.execution(i).call.call_name].parallel_safe)
        {
            return Ok(false);
        }
        let journal = self.journal.clone();
        let mut pending = indices.into_iter();
        let mut running = FuturesUnordered::new();
        let mut failure = None;
        loop {
            while running.len() < limit
                && failure.is_none()
                && self.context.check_cancelled().is_ok()
                && self.state.interactions.is_empty()
            {
                let Some(index) = pending.next() else {
                    break;
                };
                let execution = self.execution(index).clone();
                // Persist only the calls occupying a dispatch slot. Queued calls
                // remain Planned/Approved and become NotExecuted on cancellation.
                self.checkpoint = journal.checkpoint.lock().await.clone();
                self.execution_mut(index).state = ToolExecutionState::Started;
                self.save(Some(AgentEvent::ToolStarted {
                    tool_id: execution.tool_id.clone(),
                    call_id: execution.call.id.clone(),
                }))
                .await?;
                let binding = self.tools.by_name[&execution.call.call_name].clone();
                let mut context = crate::tool_batches::invocation_context(self.context, &execution);
                context.budget_journal = Some(journal.clone());
                let invoker = self.services.tool_invoker.clone();
                let timeout = self.state.request.limits.tool_timeout;
                running.push(async move {
                    let result = invoker
                        .invoke(
                            &execution.invocation_id.to_string(),
                            &execution.call,
                            &binding,
                            &context,
                            timeout,
                        )
                        .await;
                    (index, result)
                });
            }
            let Some((index, result)) = running.next().await else {
                break;
            };
            // Share the revision with concurrent budget reservations. Never let
            // an older journal checkpoint overwrite a completed sibling.
            self.checkpoint = journal.checkpoint.lock().await.clone();
            let saved = self.finish_tool_result(index, result).await;
            if let Err(error) = saved {
                if error.contains_state_error() {
                    return Err(error);
                }
                failure.get_or_insert(error);
            }
            // Even after cancellation or a persistence failure, drain in-flight
            // results before returning the error. Known successes remain known.
        }
        self.context.check_cancelled()?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(true)
    }
    async fn complete_tool_batch(&mut self) -> Result<(), AgentError> {
        let ReactCursor::Tools {
            assistant,
            executions,
        } = self.state.cursor.clone()
        else {
            unreachable!()
        };
        let mut batch = if self.state.pending_assistant_messages.is_empty() {
            // Checkpoints created before message segmentation stored only this.
            vec![assistant.as_ref().clone()]
        } else {
            std::mem::take(&mut self.state.pending_assistant_messages)
        };
        let mut completed = Vec::new();
        for execution in &executions {
            let ToolExecutionState::Finished(output) = execution.state.clone() else {
                return Err(AgentError::Run("unfinished tool in completed batch".into()));
            };
            completed.push((execution, output));
        }
        batch.extend(crate::tool_batches::result_messages(&completed));
        let outputs = crate::tool_batches::result_events(&assistant, &completed);
        self.checkpoint.transcript.extend(batch);
        self.state.cursor = ReactCursor::Model;
        self.save(None).await?;
        for output in outputs {
            self.context.emit_output(output);
        }
        Ok(())
    }
    fn execution(&self, index: usize) -> &ToolExecution {
        let ReactCursor::Tools { executions, .. } = &self.state.cursor else {
            unreachable!()
        };
        &executions[index]
    }
    fn execution_mut(&mut self, index: usize) -> &mut ToolExecution {
        let ReactCursor::Tools { executions, .. } = &mut self.state.cursor else {
            unreachable!()
        };
        &mut executions[index]
    }
}

/// A model projection backed by an immutable transcript snapshot. Store writes
/// remain exclusively in the ReAct's atomic checkpoint transition.
pub struct TranscriptProjection {
    key: ConversationKey,
    generation: Option<String>,
    original: Vec<Message>,
}
impl TranscriptProjection {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            key: ConversationKey::new("projection"),
            generation: None,
            original: messages,
        }
    }
    pub fn from_snapshot(snapshot: ConversationSnapshot) -> Self {
        Self {
            key: snapshot.key,
            generation: Some(snapshot.generation),
            original: snapshot.messages,
        }
    }
    pub fn with_key(mut self, key: ConversationKey) -> Self {
        self.key = key;
        self
    }
}
#[async_trait]
impl ConversationSession for TranscriptProjection {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        Ok(ConversationSnapshot::new(
            self.key.clone(),
            self.generation
                .clone()
                .unwrap_or_else(|| self.key.branch.clone()),
            self.original.clone(),
        ))
    }

    async fn append(&mut self, _: &[Message]) -> Result<(), ConversationError> {
        Err(ConversationError::Other(
            "transcript projection is read-only".into(),
        ))
    }
}

pub(crate) struct RunLeaseGuard {
    store: Arc<dyn StateStore>,
    lease: ExecutionLease,
    heartbeat: tokio::task::JoinHandle<()>,
    cleanup_timeout: Duration,
    closed: bool,
}
impl RunLeaseGuard {
    pub(crate) fn new(
        store: Arc<dyn StateStore>,
        lease: ExecutionLease,
        ttl: Duration,
        cleanup_timeout: Duration,
        context: RunContext,
    ) -> Self {
        let database = store.clone();
        let token = lease.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(ttl / 3).await;
                // Renewal refreshes advisory expiry only. The fencing identity
                // remains valid for commits/releases throughout this ownership.
                if database.renew(&token, ttl).await.is_err() {
                    context.cancellation.cancel();
                    break;
                }
            }
        });
        Self {
            store,
            lease,
            heartbeat,
            cleanup_timeout,
            closed: false,
        }
    }
    pub(crate) async fn close(mut self) -> Result<(), StateError> {
        self.heartbeat.abort();
        let result = release_lease(self.store.as_ref(), &self.lease, self.cleanup_timeout).await;
        // Controlled cleanup is complete, including any reported failure. Drop
        // is only a fallback for cancellation/panic before that result exists.
        self.closed = true;
        result
    }
}
impl Drop for RunLeaseGuard {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if self.closed {
            return;
        }
        let store = self.store.clone();
        let lease = self.lease.clone();
        let timeout = self.cleanup_timeout;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = release_lease(store.as_ref(), &lease, timeout).await;
            });
        }
    }
}

async fn release_lease(
    store: &dyn StateStore,
    lease: &ExecutionLease,
    timeout: Duration,
) -> Result<(), StateError> {
    let result = tokio::time::timeout(timeout, async {
        match store.release(lease).await {
            Err(StateError::Backend(_)) => {
                // Release is fenced and safe to retry even if the first write
                // committed before returning an error. Never retry indefinitely.
                tokio::time::sleep(Duration::from_millis(10).min(timeout / 4)).await;
                store.release(lease).await
            }
            result => result,
        }
    })
    .await
    .map_err(|_| StateError::Backend("execution lease release timed out".into()))?;
    match result {
        Ok(()) | Err(StateError::LeaseLost) => Ok(()),
        Err(error) => Err(error),
    }
}

struct ReactBudgetJournal {
    store: Arc<dyn StateStore>,
    lease: ExecutionLease,
    checkpoint: tokio::sync::Mutex<Checkpoint>,
}
#[async_trait]
impl HistoryCommitter for ReactBudgetJournal {
    async fn context_compaction(&self, status: HistoryStatus) -> Result<(), StateError> {
        let mut current = self.checkpoint.lock().await;
        let mut next = current.clone();
        let pending = next.history.steps.iter_mut().rev().find(|step| {
            step.name == CONTEXT_COMPACTION_STEP && step.status == HistoryStatus::Running
        });
        let now = unix_time_ms();
        if let Some(step) = pending {
            if status == HistoryStatus::Running {
                return Ok(());
            }
            step.status = status;
            step.finished_at_ms = Some(now);
        } else if status == HistoryStatus::Running {
            next.history.steps.push(HistoryStep {
                id: format!(
                    "context-compaction/{}/{}",
                    next.run_id,
                    next.history.steps.len()
                ),
                run_id: next.run_id,
                sequence: 0,
                attempt: 1,
                name: CONTEXT_COMPACTION_STEP.into(),
                model: None,
                status,
                usage: None,
                started_at_ms: now,
                finished_at_ms: None,
            });
        } else {
            return Ok(());
        }
        *current = self
            .store
            .commit(&self.lease, current.revision, next)
            .await?;
        Ok(())
    }
    async fn model_attempt(
        &self,
        step: HistoryStep,
        messages: Vec<HistoryMessage>,
        response: Option<ChatResponse>,
        tool_parents: std::collections::BTreeMap<RunId, String>,
    ) -> Result<(), StateError> {
        let mut current = self.checkpoint.lock().await;
        let mut next = current.clone();
        if let Some(old) = next.history.steps.iter_mut().find(|s| s.id == step.id) {
            *old = step.clone();
        } else {
            next.history.steps.push(step.clone());
        }
        for message in &messages {
            if let Some(old) = next
                .history
                .messages
                .iter_mut()
                .find(|m| m.id == message.id)
            {
                *old = message.clone();
            } else if let (Some(step_id), Some(output_index)) =
                (&message.step_id, message.output_index)
            {
                let position = next.history.messages.iter().position(|existing| {
                    existing.step_id.as_ref() == Some(step_id)
                        && existing
                            .output_index
                            .is_some_and(|index| index > output_index)
                });
                if let Some(position) = position {
                    next.history.messages.insert(position, message.clone());
                } else {
                    next.history.messages.push(message.clone());
                }
            } else {
                next.history.messages.push(message.clone());
            }
        }
        for call in response
            .as_ref()
            .map(|r| r.tool_calls.clone())
            .unwrap_or_default()
        {
            if !next
                .history
                .tool_invocations
                .iter()
                .any(|t| t.id == call.invocation_id)
            {
                // Gateway envelopes record the inner tool for durable history;
                // the update path rewrites tool_id from the execution anyway.
                let presented = presented_call(&call);
                let call = presented.as_ref().unwrap_or(&call);
                next.history.tool_invocations.push(HistoryToolInvocation {
                    batch_revision: None,
                    batch: None,
                    id: call.invocation_id,
                    run_id: step.run_id,
                    step_id: step.id.clone(),
                    message_id: tool_parents
                        .get(&call.invocation_id)
                        .cloned()
                        .or_else(|| messages.last().map(|m| m.id.clone()))
                        .ok_or_else(|| {
                            StateError::Invalid("tool call requires a history message".into())
                        })?,
                    call_id: call.id.clone(),
                    tool_id: call.call_name.clone(),
                    tool_name: call.call_name.clone(),
                    arguments: call.arguments.clone(),
                    status: HistoryStatus::Planned,
                    content: Vec::new(),
                    structured: None,
                    is_error: None,
                    result_reference: None,
                    started_at_ms: None,
                    finished_at_ms: None,
                });
            }
        }
        if let Some(response) = response {
            let mut state: ReactState = serde_json::from_value(next.state.clone())
                .map_err(|_| StateError::Invalid("invalid history ReAct state".into()))?;
            state.pending_model_response = Some(response);
            next.state = serde_json::to_value(state)
                .map_err(|_| StateError::Invalid("invalid history ReAct state".into()))?;
        }
        *current = self
            .store
            .commit(&self.lease, current.revision, next)
            .await?;
        Ok(())
    }
}
#[async_trait]
impl crate::budget::BudgetJournal for ReactBudgetJournal {
    async fn save(
        &self,
        ledger: Arc<std::sync::Mutex<crate::budget::UsageLedger>>,
        child_runs: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<(), StateError> {
        let mut current = self.checkpoint.lock().await;
        let mut next = current.clone();
        let mut state: ReactState = serde_json::from_value(next.state.clone())
            .map_err(|_| StateError::Invalid("invalid ReAct budget state".into()))?;
        // Read the ledger after acquiring the journal lock. Concurrent callers
        // cannot commit an older, smaller reservation snapshot after a newer one.
        state.budget_usage = ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .including_reservations();
        state.child_runs = child_runs.load(std::sync::atomic::Ordering::Acquire);
        next.state = serde_json::to_value(state)
            .map_err(|_| StateError::Invalid("invalid ReAct budget state".into()))?;
        *current = self
            .store
            .commit(&self.lease, current.revision, next)
            .await?;
        Ok(())
    }
}

fn admit_native_step(context: &RunContext, local_limit: usize) -> bool {
    let admitted = if let Some(budget) = context.extensions.get::<ContinuationBudget>() {
        budget.admit_step(context.execution_counters.as_ref(), local_limit)
    } else {
        context.execution_counters.as_ref().is_none_or(|counter| {
            crate::budget::ExecutionCounters::admit(&counter.steps, 1, local_limit)
        })
    };
    if admitted && let Some(counter) = context.extensions.get::<ModelDispatchCounter>() {
        counter.record_step();
    }
    admitted
}
fn admit_native_tools(context: &RunContext, local_limit: usize, amount: usize) -> bool {
    let admitted = if let Some(budget) = context.extensions.get::<ContinuationBudget>() {
        budget.admit_tools(context.execution_counters.as_ref(), local_limit, amount)
    } else {
        context.execution_counters.as_ref().is_none_or(|counter| {
            crate::budget::ExecutionCounters::admit(&counter.tools, amount, local_limit)
        })
    };
    if admitted && let Some(counter) = context.extensions.get::<ModelDispatchCounter>() {
        counter.record_tools(amount);
    }
    admitted
}

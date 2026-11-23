//! Expansion and provider-result pairing for batch-only gateway tools.
use crate::*;
use std::collections::HashSet;

type ExpandedCall = (ToolCall, Option<ToolBatchItem>, Option<String>);

pub(crate) fn expand_calls(
    calls: &[ToolCall],
    tools: &crate::react::ToolSet,
) -> Result<Vec<ExpandedCall>, AgentError> {
    let mut ids = HashSet::new();
    let mut expanded = Vec::new();
    for call in calls {
        if call.id.is_empty() || !ids.insert(&call.id) {
            return Err(AgentError::Run(
                "model returned invalid tool call IDs".into(),
            ));
        }
        let envelope = (tools.gateway && call.call_name == TOOL_EXECUTE_CALL_NAME)
            .then(|| GatewayEnvelope::parse(&call.arguments).ok())
            .flatten();
        let name = envelope
            .as_ref()
            .map_or(call.call_name.as_str(), |e| e.tool.as_str());
        let binding = tools.by_name.get(name);
        // The gateway trio's own batch tools and whitelisted native tools use
        // the same items contract as wrapped tools. Envelopes keep addressing
        // inner tools by their call name.
        let gateway_batch_tool = matches!(
            call.call_name.as_str(),
            TOOL_SEARCH_CALL_NAME | TOOL_SCHEMA_CALL_NAME
        );
        let native_batch_tool = binding.is_some_and(|b| tools.is_native(&b.descriptor.id));
        let batch = ((!tools.gateway && binding.is_some_and(|b| b.executor.direct_batch_input()))
            || envelope.is_some()
            || gateway_batch_tool
            || native_batch_tool)
            && binding.is_some_and(|b| b.descriptor.input_mode == ToolInputMode::Batch);
        if !batch {
            let error = (tools.gateway
                && call.call_name == TOOL_SEARCH_CALL_NAME
                && call.arguments.get("tool").is_some())
            .then(|| tool_search_legacy_error().to_owned());
            expanded.push((call.clone(), None, error));
            continue;
        }
        let arguments = envelope.as_ref().map_or(&call.arguments, |e| &e.arguments);
        match tool_batch_items(arguments) {
            Ok(items) => {
                for (index, arguments) in items.iter().enumerate() {
                    let invocation_id = RunId::new_v4();
                    let mut child = call.clone();
                    child.invocation_id = invocation_id;
                    child.id = invocation_id.to_string();
                    child.arguments = match &envelope {
                        Some(e) => serde_json::json!({"tool": e.tool, "arguments": arguments}),
                        None => arguments.clone(),
                    };
                    // The removed search-definition form contained `tool` in
                    // each item. Preserve the per-item error so valid siblings
                    // still run while the model learns the new entry point.
                    let error = (tools.gateway
                        && call.call_name == TOOL_SEARCH_CALL_NAME
                        && arguments.get("tool").is_some())
                    .then(|| tool_search_legacy_error().to_owned());
                    expanded.push((
                        child,
                        Some(ToolBatchItem {
                            invocation_id,
                            call_id: invocation_id.to_string(),
                            parent_call_id: call.id.clone(),
                            parent_invocation_id: call.invocation_id,
                            index,
                        }),
                        error,
                    ));
                }
            }
            Err(error) => expanded.push((call.clone(), None, Some(error))),
        }
    }
    Ok(expanded)
}

/// One provider result per original call; child IDs belong only to execution,
/// audit and interaction records, never to the provider transcript.
fn grouped_results(completed: &[(&ToolExecution, ToolOutput)]) -> Vec<(String, ToolOutput)> {
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    for (execution, output) in completed {
        let (id, output) = if let Some(item) = &execution.batch_item {
            if !seen.insert(&item.parent_call_id) {
                continue;
            }
            let items = completed.iter().filter_map(|(e, output)| {
                e.batch_item
                    .as_ref()
                    .filter(|i| i.parent_call_id == item.parent_call_id)
                    .map(|i| ToolBatchItemResult::finished(i.clone(), output))
            });
            (item.parent_call_id.clone(), tool_batch_output(items))
        } else {
            (execution.call.id.clone(), output.clone())
        };
        results.push((id, output));
    }
    results
}

pub(crate) fn result_messages(completed: &[(&ToolExecution, ToolOutput)]) -> Vec<Message> {
    grouped_results(completed)
        .into_iter()
        .map(|(id, output)| {
            let mut message = Message::tool(id.clone(), output.content);
            message.tool_result = Some(ToolResultMetadata {
                is_error: output.is_error,
                // Batch content already contains the complete compact model result.
                // Full structured data belongs to events/history; including it
                // here repeats results and injects audit IDs/media into prompts.
                structured: if completed.iter().any(|(e, _)| {
                    e.batch_item
                        .as_ref()
                        .is_some_and(|b| b.parent_call_id == id)
                }) {
                    None
                } else {
                    output.structured
                },
                result_reference: output.result_reference,
            });
            message
        })
        .collect()
}

pub(crate) fn result_events(
    assistant: &Message,
    completed: &[(&ToolExecution, ToolOutput)],
) -> Vec<OutputEvent> {
    grouped_results(completed)
        .into_iter()
        .filter_map(|(id, output)| {
            let call = assistant.tool_calls.iter().find(|c| c.id == id)?;
            Some(OutputEvent::ToolResult {
                call: presented_call(call).unwrap_or_else(|| call.clone()),
                output,
            })
        })
        .collect()
}

pub(crate) fn update_batch_history(
    executions: &[ToolExecution],
    history: &mut HistoryRecords,
    now: u64,
    terminal: bool,
) {
    // Keep the model's immutable aggregate arguments, and create independent
    // children for per-item approvals, middleware, result processing and audit.
    for execution in executions {
        let Some(item) = &execution.batch_item else {
            continue;
        };
        if history
            .tool_invocations
            .iter()
            .any(|r| r.id == execution.invocation_id)
        {
            continue;
        }
        let Some(mut child) = history
            .tool_invocations
            .iter()
            .find(|r| r.id == item.parent_invocation_id)
            .cloned()
        else {
            continue;
        };
        child.id = execution.invocation_id;
        child.batch = Some(item.clone());
        child.batch_revision = None;
        child.call_id = execution.call.id.clone();
        child.tool_id = execution.tool_id.clone();
        child.tool_name = execution.call.call_name.clone();
        child.arguments = execution.call.arguments.clone();
        child.status = HistoryStatus::Planned;
        child.content.clear();
        child.structured = None;
        child.is_error = None;
        child.result_reference = None;
        child.started_at_ms = None;
        child.finished_at_ms = None;
        history.tool_invocations.push(child);
    }
    let parents: HashSet<_> = executions
        .iter()
        .filter_map(|e| e.batch_item.as_ref().map(|i| i.parent_invocation_id))
        .collect();
    for parent in parents {
        let Some(record) = history.tool_invocations.iter_mut().find(|r| r.id == parent) else {
            continue;
        };
        if record.is_error.is_some() {
            continue;
        }
        let children: Vec<_> = executions
            .iter()
            .filter(|e| {
                e.batch_item
                    .as_ref()
                    .is_some_and(|i| i.parent_invocation_id == parent)
            })
            .collect();
        if let Some(first) = children.first() {
            record.tool_id = first.tool_id.clone();
        }
        let previous_status = record.status.clone();
        let output = execution_batch_output(&children, terminal);
        let content = history_content(&output.content);
        let changed = record.content != content || record.structured != output.structured;
        record.content = content;
        record.structured = output.structured;
        if children
            .iter()
            .all(|e| matches!(e.state, ToolExecutionState::Finished(_)))
        {
            record.status = if output.is_error {
                HistoryStatus::Failed
            } else {
                HistoryStatus::Completed
            };
            record.is_error = Some(output.is_error);
            record.finished_at_ms = Some(now);
        } else if terminal {
            record.status = HistoryStatus::Unknown;
            record.is_error = Some(output.is_error);
            record.finished_at_ms.get_or_insert(now);
        } else if children
            .iter()
            .any(|e| matches!(e.state, ToolExecutionState::WaitingApproval { .. }))
        {
            record.status = HistoryStatus::Waiting;
        } else {
            record.status = HistoryStatus::Running;
            record.started_at_ms.get_or_insert(now);
        }
        if record.batch_revision.is_none() || changed || previous_status != record.status {
            let revision = record
                .batch_revision
                .as_deref()
                .unwrap_or("0")
                .parse::<u64>()
                .expect("native batch revision");
            record.batch_revision = Some(
                revision
                    .checked_add(1)
                    .expect("batch revision overflow")
                    .to_string(),
            );
        }
    }
}

/// Per-item context carries transport ancestry without changing atomic call IDs.
pub(crate) fn invocation_context(run: &RunContext, execution: &ToolExecution) -> RunContext {
    let mut context = run.clone();
    // Child agents can inherit host extensions from a spawning tool. A single
    // call in that child must never inherit the parent's transport identity.
    context.extensions.remove::<ToolBatchItem>();
    context.extensions.remove::<PreparedToolInvocation>();
    if let Some(preparation) = &execution.preparation {
        context
            .extensions
            .insert(PreparedToolInvocation(preparation.clone()));
    }
    if let Some(item) = &execution.batch_item {
        context.extensions.insert(item.clone());
    }
    context
}

/// A single projector owns both durable history and interrupted live results.
fn execution_batch_output(executions: &[&ToolExecution], terminal: bool) -> ToolOutput {
    tool_batch_output(executions.iter().map(|execution| {
        let identity = execution.batch_item.as_ref().expect("batch child").clone();
        match &execution.state {
            ToolExecutionState::Finished(output) => ToolBatchItemResult::finished(identity, output),
            state => ToolBatchItemResult::pending(
                identity,
                match state {
                    ToolExecutionState::Started if terminal => ToolBatchStatus::Unknown,
                    ToolExecutionState::Started => ToolBatchStatus::Running,
                    _ if terminal => ToolBatchStatus::NotExecuted,
                    ToolExecutionState::WaitingApproval { .. } => ToolBatchStatus::Waiting,
                    _ => ToolBatchStatus::Pending,
                },
            ),
        }
    }))
}

/// Project a stopped batch once for model recovery and live results. Original
/// provider call IDs remain paired, even when some children never ran.
fn stopped_results(
    assistant: &Message,
    executions: &[ToolExecution],
) -> Vec<(ToolCall, ToolOutput, bool)> {
    assistant.tool_calls.iter().filter_map(|call| {
        let children: Vec<_> = executions.iter().filter(|e|
            e.batch_item.as_ref().is_some_and(|i| i.parent_invocation_id == call.invocation_id)
        ).collect();
        if !children.is_empty() {
            return Some((call.clone(), execution_batch_output(&children, true), true));
        }
        let execution = executions.iter().find(|e| e.invocation_id == call.invocation_id)?;
        let output = match &execution.state {
            ToolExecutionState::Finished(output) => output.clone(),
            ToolExecutionState::Started => ToolOutput::error("Tool execution was interrupted; outcome unknown. Verify its effects before retrying."),
            _ => ToolOutput::error("Tool was not executed before the run stopped."),
        };
        Some((call.clone(), output, false))
    }).collect()
}

pub(crate) fn interrupted_result_messages(
    assistant: &Message,
    executions: &[ToolExecution],
) -> Vec<Message> {
    stopped_results(assistant, executions)
        .into_iter()
        .map(|(call, output, batch)| {
            let mut message = Message::tool(call.id, output.content);
            message.tool_result = Some(ToolResultMetadata {
                is_error: output.is_error,
                structured: if batch { None } else { output.structured },
                result_reference: output.result_reference,
            });
            message
        })
        .collect()
}

/// On failure/cancellation, publish the same outcomes used by model recovery.
pub(crate) fn interrupted_result_events(
    assistant: &Message,
    executions: &[ToolExecution],
) -> Vec<OutputEvent> {
    stopped_results(assistant, executions)
        .into_iter()
        .map(|(call, output, _)| OutputEvent::ToolResult {
            call: presented_call(&call).unwrap_or(call),
            output,
        })
        .collect()
}

/// Replaceable progress, not completion. All siblings, including policy/schema
/// failures and in-flight results, are projected exactly as in durable history.
pub(crate) fn batch_snapshot_events(
    assistant: &Message,
    executions: &[ToolExecution],
    history: &HistoryRecords,
) -> Vec<OutputEvent> {
    assistant
        .tool_calls
        .iter()
        .filter_map(|call| {
            let children: Vec<_> = executions
                .iter()
                .filter(|e| {
                    e.batch_item
                        .as_ref()
                        .is_some_and(|i| i.parent_invocation_id == call.invocation_id)
                })
                .collect();
            if children.is_empty()
                || children
                    .iter()
                    .all(|e| matches!(e.state, ToolExecutionState::Finished(_)))
            {
                return None;
            }
            let call = presented_call(call).unwrap_or_else(|| call.clone());
            let revision = history
                .tool_invocations
                .iter()
                .find(|r| r.id == call.invocation_id)
                .and_then(|r| r.batch_revision.clone())
                .expect("committed batch revision");
            let snapshot = ToolBatchSnapshot {
                revision,
                tool_call_id: call.invocation_id,
                tool_name: call.call_name,
                arguments: call.arguments,
                output: execution_batch_output(&children, false),
            };
            Some(OutputEvent::ActivitySnapshot {
                id: format!("tool-batch/{}", snapshot.tool_call_id),
                activity_type: TOOL_BATCH_ACTIVITY_TYPE.into(),
                content: serde_json::to_value(snapshot)
                    .expect("batch snapshot serializes")
                    .as_object()
                    .unwrap()
                    .clone(),
            })
        })
        .collect()
}

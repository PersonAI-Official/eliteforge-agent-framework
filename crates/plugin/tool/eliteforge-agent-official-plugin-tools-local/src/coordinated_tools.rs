//! File tools use one coordination boundary, including direct executor calls.
use crate::{
    coordination::*,
    edits::{self, Change},
    filesystem::FileSystemRoot,
};
use async_trait::async_trait;
use eliteforge_agent_core::{
    PreparedToolInvocation, RunContext, Tool, ToolBinding, ToolDescriptor, ToolError, ToolOutput,
    ToolPreparation, ToolResourceAccess,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct ActiveFileLease(pub Arc<dyn FileCoordinationLease>);
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PreparedFileEdit {
    tool: String,
    arguments_hash: String,
    subject: FileSubject,
    revision: String,
    resources: Vec<FileResource>,
    observations: Vec<FileObservation>,
}
fn fail(s: impl ToString) -> ToolError {
    ToolError::Failed(s.to_string())
}
impl PreparedFileEdit {
    pub(crate) fn observation(
        &self,
        root: &FileSystemRoot,
        path: &str,
    ) -> Result<&FileObservation, ToolError> {
        let resource = root.resource(path)?;
        self.observations
            .iter()
            .find(|o| o.resource == resource)
            .ok_or_else(|| {
                fail(format!(
                    "notApplied: no read observation for {path}; read the relevant content first"
                ))
            })
    }
    pub(crate) fn require_current(
        &self,
        root: &FileSystemRoot,
        path: &str,
        bytes: &[u8],
        complete: bool,
    ) -> Result<(), ToolError> {
        let observation = self.observation(root, path)?;
        if complete && (!observation.complete || observation.fingerprint != edits::version(bytes)) {
            return Err(fail(format!(
                "notApplied: complete current read required for {path}; re-read the file"
            )));
        }
        Ok(())
    }
    pub(crate) fn require_seen(
        &self,
        root: &FileSystemRoot,
        path: &str,
        text: &str,
    ) -> Result<(), ToolError> {
        let observation = self.observation(root, path)?;
        let text = text.replace("\r\n", "\n");
        if text.is_empty() {
            return Err(fail("empty context does not establish a read credential"));
        }
        // Adjacent returned pages may form a single context, without inventing
        // content from gaps. All fragments in one observation have one version.
        let mut fragments = observation.fragments.clone();
        fragments.sort_by_key(|f| f.start_line);
        let mut through = 0;
        let mut view = String::new();
        for fragment in fragments {
            if fragment.start_line > through + 1 {
                view.clear();
            }
            if fragment.start_line > through || view.is_empty() {
                if !view.is_empty() {
                    view.push('\n');
                }
                view.push_str(&fragment.text);
            } else {
                // Overlapping pages: checking each separately is conservative.
                if fragment.text.contains(&text) {
                    return Ok(());
                }
            }
            through = through.max(fragment.end_line);
            if view.contains(&text) {
                return Ok(());
            }
        }
        Err(fail(format!(
            "notApplied: edit context was not returned by a read of {path}; read that region first"
        )))
    }
}
struct FileTool {
    root: FileSystemRoot,
    descriptor: ToolDescriptor,
    reader: Option<Arc<dyn Tool>>,
}
pub(crate) fn binding(
    descriptor: ToolDescriptor,
    root: FileSystemRoot,
    reader: Option<Arc<dyn Tool>>,
) -> ToolBinding {
    ToolBinding::new(
        descriptor.clone(),
        FileTool {
            root,
            descriptor,
            reader,
        },
    )
    .parallel_safe()
    .with_domain_code("fs")
}
fn subject(run: &RunContext) -> FileSubject {
    run.extensions
        .get::<FileCoordinationIdentity>()
        .map(|i| i.0.clone())
        .unwrap_or_else(|| FileSubject {
            security_domain: "standalone".into(),
            session_id: run.session_id.clone(),
            task_instance_id: run.run_id.to_string(),
        })
}
fn revision(root: &FileSystemRoot, run: &RunContext) -> String {
    run.extensions
        .get::<crate::LocalToolScopeRevision>()
        .map(|r| r.0.clone())
        .unwrap_or_else(|| root.authorization_revision())
}
fn paths(
    root: &FileSystemRoot,
    descriptor: &ToolDescriptor,
    arguments: &Value,
) -> Result<Vec<FileResource>, ToolError> {
    let mut paths = Vec::new();
    if descriptor.id == "filesystem.apply_patch" && arguments["mode"] == "patch" {
        let cwd = root.resolve_directory(arguments.get("cwd").and_then(Value::as_str))?;
        let patch = arguments["patch"]
            .as_str()
            .ok_or_else(|| fail("patch is required"))?;
        for operation in crate::text_patch::parse_text_patch(patch).map_err(fail)? {
            let (path, destination) = match operation {
                crate::text_patch::FilePatch::Add { path, .. }
                | crate::text_patch::FilePatch::Delete { path } => (path, None),
                crate::text_patch::FilePatch::Update {
                    path, destination, ..
                } => (path, destination),
            };
            for path in std::iter::once(path).chain(destination) {
                let path = root.display(&cwd.root, &cwd.path.join(path));
                paths.push(root.resource(&path)?);
            }
        }
    } else if let Some(path) = arguments.get("path").and_then(Value::as_str) {
        paths.push(root.resource(path)?);
    } else if let Some(reference) = arguments.get("fileRef")
        && reference["kind"] == "workspace_path"
    {
        paths.push(
            root.resource(
                reference["path"]
                    .as_str()
                    .ok_or_else(|| fail("fileRef path required"))?,
            )?,
        );
    }
    if paths.len() > 32 {
        return Err(fail("at most 32 affected paths per edit"));
    }
    let mut keys = std::collections::BTreeSet::new();
    for resource in &paths {
        if !keys.insert(resource.physical_alias.clone()) {
            return Err(fail("duplicate file operation"));
        }
    }
    Ok(paths)
}
#[async_trait]
impl Tool for FileTool {
    fn direct_batch_input(&self) -> bool {
        self.descriptor.input_mode == eliteforge_agent_core::ToolInputMode::Batch
    }
    async fn prepare(
        &self,
        arguments: &Value,
        run: &RunContext,
    ) -> Result<Option<ToolPreparation>, ToolError> {
        let validator =
            eliteforge_agent_core::compile_schema(&self.descriptor.input_schema).map_err(fail)?;
        if !validator.is_valid(arguments) {
            return Err(fail("file arguments do not match the current protocol"));
        }
        let resources = paths(&self.root, &self.descriptor, arguments)?;
        let subject = subject(run);
        let revision = revision(&self.root, run);
        let backend = self.root.coordination.backend()?;
        let observations = tokio::select! {
            biased;
            _ = run.cancelled() => return Err(fail("file preparation cancelled")),
            result = tokio::time::timeout_at(run.deadline, backend.observations(&subject, &revision, &resources)) => result.map_err(|_| fail("file preparation deadline exceeded"))??,
        };
        let prepared = PreparedFileEdit {
            tool: self.descriptor.id.clone(),
            arguments_hash: edits::version(arguments.to_string().as_bytes()),
            subject,
            revision,
            resources: resources.clone(),
            observations,
        };
        Ok(Some(ToolPreparation {
            resources: resources
                .iter()
                .flat_map(|r| {
                    [
                        format!("file:{}", r.physical_alias),
                        format!("resource:{}", r.key()),
                    ]
                })
                .map(|key| ToolResourceAccess {
                    key,
                    write: self.reader.is_none(),
                })
                .collect(),
            state: serde_json::to_value(prepared).map_err(fail)?,
        }))
    }
    async fn call(&self, arguments: Value, run: &RunContext) -> Result<ToolOutput, ToolError> {
        if self.descriptor.input_mode == eliteforge_agent_core::ToolInputMode::Batch
            && run
                .extensions
                .get::<eliteforge_agent_core::ToolBatchItem>()
                .is_none()
        {
            let items = eliteforge_agent_core::tool_batch_items(&arguments).map_err(fail)?;
            let mut prepared = Vec::new();
            for item in items {
                prepared.push(self.prepare(item, run).await.map_err(|e| e.to_string()));
            }
            let mut outputs = Vec::new();
            let parent = eliteforge_agent_core::RunId::new_v4();
            for (index, item) in items.iter().enumerate() {
                let id = eliteforge_agent_core::RunId::new_v4();
                let identity = eliteforge_agent_core::ToolBatchItem {
                    invocation_id: id,
                    call_id: id.to_string(),
                    parent_call_id: parent.to_string(),
                    parent_invocation_id: parent,
                    index,
                };
                let mut child = run.clone();
                child.extensions.insert(identity.clone());
                if let Ok(Some(state)) = &prepared[index] {
                    child
                        .extensions
                        .insert(PreparedToolInvocation(state.clone()));
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
                    match Box::pin(self.call(item.clone(), &child)).await {
                        Ok(output) => output,
                        Err(error) => ToolOutput::error(error.to_string()),
                    }
                };
                outputs.push(eliteforge_agent_core::ToolBatchItemResult::finished(
                    identity, &output,
                ));
            }
            return Ok(eliteforge_agent_core::tool_batch_output(outputs));
        }
        let preparation = if let Some(prepared) = run.extensions.get::<PreparedToolInvocation>() {
            prepared.0.clone()
        } else {
            self.prepare(&arguments, run)
                .await?
                .ok_or_else(|| fail("file preparation missing"))?
        };
        let prepared: PreparedFileEdit = serde_json::from_value(preparation.state).map_err(fail)?;
        if prepared.tool != self.descriptor.id
            || prepared.arguments_hash != edits::version(arguments.to_string().as_bytes())
            || prepared.subject != subject(run)
            || prepared.revision != revision(&self.root, run)
        {
            return Err(fail("file preparation identity or authority changed"));
        }
        let backend = self.root.coordination.backend()?;
        let lease = backend
            .acquire_many(prepared.resources.clone(), run)
            .await?;
        lease.validate().await?;
        let current_resources = paths(&self.root, &self.descriptor, &arguments)?;
        if current_resources != prepared.resources
            || current_resources
                .iter()
                .zip(&prepared.resources)
                .any(|(a, b)| a.physical_alias != b.physical_alias)
        {
            return Err(fail("file authority or resolved path changed"));
        }
        let mut invocation = run.clone();
        invocation.extensions.insert(ActiveFileLease(lease.clone()));
        let operation = async {
            if let Some(reader) = &self.reader {
                let output = reader.call(arguments.clone(), &invocation).await?;
                if let Some(resource) = prepared.resources.first() {
                    let data = output
                        .structured
                        .as_ref()
                        .ok_or_else(|| fail("read result lacks structured content"))?;
                    // The reader already validated one stable snapshot. Record
                    // precisely that result instead of reading/hash-computing it twice.
                    let fingerprint = data["version"]
                        .as_str()
                        .ok_or_else(|| fail("read result lacks fingerprint"))?
                        .to_owned();
                    let total_lines = data["totalLines"]
                        .as_u64()
                        .ok_or_else(|| fail("read result lacks totalLines"))?
                        as usize;
                    let start_line = data["startLine"].as_u64().unwrap_or(1) as usize;
                    let end_line = data["endLine"].as_u64().unwrap_or(0) as usize;
                    let text = data["content"].as_str().unwrap_or("").to_owned();
                    // A partially returned line is usable as context but must
                    // never count as full-line/full-file coverage.
                    let complete = !data["truncated"].as_bool().unwrap_or(true)
                        && start_line == 1
                        && end_line >= total_lines;
                    let fragments = if end_line >= start_line {
                        vec![FileFragment {
                            start_line,
                            end_line,
                            text,
                        }]
                    } else {
                        Vec::new()
                    };
                    backend
                        .record_read(
                            lease.as_ref(),
                            FileObservation {
                                resource: resource.clone(),
                                subject: prepared.subject.clone(),
                                authorization_revision: prepared.revision.clone(),
                                fingerprint,
                                total_lines,
                                complete,
                                fragments,
                            },
                        )
                        .await?;
                }
                return Ok(output);
            }
            let root = self.root.clone();
            let args = arguments.clone();
            let ctx = invocation.clone();
            let basis = prepared.clone();
            let id = self.descriptor.id.clone();
            let result =
                crate::tools::blocking(move || edit(&root, &id, &args, &ctx, &basis)).await;
            let (mut output, changes, dry_run) = match result {
                Ok(value) => value,
                Err(error) => {
                    let status = match &error {
                        ToolError::Indeterminate(_) => FileEditStatus::Indeterminate,
                        ToolError::Failed(s) if s.contains("rolled back") => {
                            FileEditStatus::RolledBack
                        }
                        _ => FileEditStatus::NotApplied,
                    };
                    if !matches!(status, FileEditStatus::NotApplied) {
                        let _ = backend
                            .invalidate(FileInvalidation {
                                subject: Some(prepared.subject.clone()),
                                resources: prepared.resources.clone(),
                                ..Default::default()
                            })
                            .await;
                    }
                    let mut output = crate::tools::json_output(
                        json!({"status":status,"paths":prepared.resources.iter().map(|r| &r.path).collect::<Vec<_>>(),"error":error.to_string()}),
                    )?;
                    output.is_error = true;
                    return Ok(output);
                }
            };
            if !dry_run {
                let publication =
                    match changed_observations(&self.root, &prepared, &changes, &arguments) {
                        Ok(observations) => {
                            backend
                                .publish(
                                    lease.as_ref(),
                                    FileEditOutcome {
                                        subject: prepared.subject.clone(),
                                        status: FileEditStatus::Applied,
                                        resources: prepared.resources.clone(),
                                        observations,
                                    },
                                )
                                .await
                        }
                        Err(error) => Err(error),
                    };
                if let Err(error) = publication {
                    let _ = backend
                        .invalidate(FileInvalidation {
                            subject: Some(prepared.subject.clone()),
                            resources: prepared.resources.clone(),
                            ..Default::default()
                        })
                        .await;
                    output = crate::tools::json_output(
                        json!({"status":"applied","observationUpdateFailed":true,"paths":prepared.resources.iter().map(|r| &r.path).collect::<Vec<_>>(),"error":error.to_string()}),
                    )?;
                    output.is_error = true;
                }
            }
            Ok(output)
        };
        // No async cancellation may detach a committing blocking worker. The
        // worker owns its lease through ActiveFileLease until recovery finishes.
        let result = operation.await;
        let released = lease.release().await;
        match (result, released) {
            (Ok(output), Ok(())) => Ok(output),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(ToolError::Indeterminate(format!(
                "file result obtained but lease release failed: {error}"
            ))),
        }
    }
}

fn edit(
    root: &FileSystemRoot,
    id: &str,
    args: &Value,
    run: &RunContext,
    prepared: &PreparedFileEdit,
) -> Result<(ToolOutput, Vec<Change>, bool), ToolError> {
    edits::ensure_active(run)?;
    let dry_run = args.get("dryRun").and_then(Value::as_bool).unwrap_or(false);
    let changes = if id == "filesystem.write_text" {
        let path = args["path"].as_str().ok_or_else(|| fail("path required"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| fail("content required"))?;
        let resolved = root.resolve(path)?;
        let before = edits::read(&resolved.path, root.max_edit_bytes())?;
        let mode = args["mode"].as_str().ok_or_else(|| fail("mode required"))?;
        match mode {
            "create" if before.is_none() => {}
            "overwrite" => prepared.require_current(
                root,
                path,
                before
                    .as_deref()
                    .ok_or_else(|| fail("overwrite requires an existing file"))?,
                true,
            )?,
            "append" if before.is_some() => {}
            _ => return Err(fail("file existence does not match write mode")),
        }
        let mut after = if mode == "append" {
            before.clone().unwrap_or_default()
        } else {
            Vec::new()
        };
        after.extend_from_slice(content.as_bytes());
        edits::text(&after)?;
        vec![Change {
            path: root.display(&resolved.root, &resolved.path),
            before,
            after: Some(after),
            permissions: None,
        }]
    } else if args["mode"] == "text" {
        let path = args["path"].as_str().ok_or_else(|| fail("path required"))?;
        let old = args["oldText"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| fail("nonempty oldText required"))?;
        let new = args["newText"]
            .as_str()
            .ok_or_else(|| fail("newText required"))?;
        let resolved = root.resolve(path)?;
        let before = edits::read(&resolved.path, root.max_edit_bytes())?
            .ok_or_else(|| fail("file does not exist"))?;
        prepared.require_current(root, path, &before, false)?;
        prepared.require_seen(root, path, old)?;
        let source = edits::text(&before)?;
        let crlf = source.contains("\r\n") && !source.replace("\r\n", "").contains('\n');
        let old = if crlf {
            old.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            old.to_owned()
        };
        let new = if crlf {
            new.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            new.to_owned()
        };
        let count = source.matches(&old).count();
        let replace_all = args["replaceAll"].as_bool().unwrap_or(false);
        let expected = args["expectedCount"].as_u64().map(|v| v as usize);
        if count == 0
            || (!replace_all && count != 1)
            || (replace_all && expected != Some(count))
            || expected.is_some_and(|e| e != count)
        {
            return Err(fail(format!(
                "context conflict in {path}: found {count} exact matches; re-read the relevant region"
            )));
        }
        let after = if replace_all {
            source.replace(&old, &new)
        } else {
            source.replacen(&old, &new, 1)
        };
        vec![Change {
            path: root.display(&resolved.root, &resolved.path),
            before: Some(before),
            after: Some(after.into_bytes()),
            permissions: None,
        }]
    } else {
        crate::patch_envelope::changes(
            root,
            args["patch"]
                .as_str()
                .ok_or_else(|| fail("patch required"))?,
            args.get("cwd").and_then(Value::as_str),
            run,
            prepared,
        )?
    };
    if changes.len() > 32
        || changes.iter().any(|c| {
            c.after
                .as_ref()
                .is_some_and(|b| b.len() > root.max_edit_bytes())
        })
    {
        return Err(fail("file edit exceeds operation limits"));
    }
    let previews = changes
        .iter()
        .map(edits::preview)
        .collect::<Result<Vec<_>, _>>()?;
    if !dry_run {
        let _parents = edits::prepare_parents(root, &changes)?;
        edits::commit(root, &changes, Some(run))?;
    }
    Ok((
        crate::tools::json_output(
            json!({"status":if dry_run {"notApplied"} else {"applied"},"dryRun":dry_run,"changes":previews}),
        )?,
        changes,
        dry_run,
    ))
}
fn changed_observations(
    root: &FileSystemRoot,
    prepared: &PreparedFileEdit,
    changes: &[Change],
    arguments: &Value,
) -> Result<Vec<FileObservation>, ToolError> {
    let operations = arguments
        .get("patch")
        .and_then(Value::as_str)
        .map(crate::text_patch::parse_text_patch)
        .transpose()
        .map_err(fail)?
        .unwrap_or_default();
    let cwd = root.resolve_directory(arguments.get("cwd").and_then(Value::as_str))?;
    let mut observations = Vec::new();
    for change in changes {
        let Some(after) = &change.after else {
            continue;
        };
        let resource = root.resource(&change.path)?;
        let mut origin = resource.clone();
        for operation in &operations {
            if let crate::text_patch::FilePatch::Update {
                path,
                destination: Some(destination),
                ..
            } = operation
            {
                let target =
                    root.resource(&root.display(&cwd.root, &cwd.path.join(destination)))?;
                if target == resource {
                    origin = root.resource(&root.display(&cwd.root, &cwd.path.join(path)))?;
                }
            }
        }
        let prior = prepared.observations.iter().find(|o| o.resource == origin);
        let before = if origin == resource {
            change.before.as_ref()
        } else {
            changes
                .iter()
                .find(|c| root.resource(&c.path).is_ok_and(|r| r == origin))
                .and_then(|c| c.before.as_ref())
        };
        let full = arguments["mode"] == "overwrite"
            || (change.before.is_none() && origin == resource)
            || prior.is_some_and(|o| {
                o.complete && before.is_some_and(|b| o.fingerprint == edits::version(b))
            });
        let text = edits::text(after)?;
        let total_lines = text.lines().count();
        // For local edits, only unchanged previously returned fragments carry
        // over. Full baselines are retained only when no intervening edit existed.
        let fragments = if full {
            vec![FileFragment {
                start_line: 1,
                end_line: total_lines,
                text: text.lines().collect::<Vec<_>>().join("\n"),
            }]
        } else {
            prior
                .map(|o| {
                    o.fragments
                        .iter()
                        .filter_map(|f| {
                            let normalized = text.replace("\r\n", "\n");
                            if normalized.matches(&f.text).count() != 1 || f.text.is_empty() {
                                return None;
                            }
                            let offset = normalized.find(&f.text)?;
                            let start_line =
                                normalized[..offset].bytes().filter(|b| *b == b'\n').count() + 1;
                            Some(FileFragment {
                                start_line,
                                end_line: if f.end_line < f.start_line {
                                    start_line.saturating_sub(1)
                                } else {
                                    start_line + f.end_line - f.start_line
                                },
                                text: f.text.clone(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut fragments = fragments;
        if !full {
            let mut established = Vec::new();
            if arguments["mode"] == "text" {
                if let Some(text) = arguments["newText"].as_str().filter(|s| !s.is_empty()) {
                    established.push(text.replace("\r\n", "\n"));
                }
            } else if arguments["mode"] == "append" {
                if let Some(text) = arguments["content"].as_str().filter(|s| !s.is_empty()) {
                    established.push(text.replace("\r\n", "\n"));
                }
            } else {
                for operation in &operations {
                    if let crate::text_patch::FilePatch::Update {
                        path,
                        destination,
                        hunks,
                    } = operation
                    {
                        let raw = root.display(
                            &cwd.root,
                            &cwd.path.join(destination.as_ref().unwrap_or(path)),
                        );
                        if root.resource(&raw)? == resource {
                            for hunk in hunks {
                                if !hunk.after.is_empty() {
                                    established.push(hunk.after.join("\n"));
                                }
                            }
                        }
                    }
                }
            }
            let normalized = text.replace("\r\n", "\n");
            for snippet in established {
                if normalized.matches(&snippet).count() == 1 {
                    let offset = normalized.find(&snippet).expect("unique snippet");
                    let start_line =
                        normalized[..offset].bytes().filter(|b| *b == b'\n').count() + 1;
                    // Only whole returned lines count toward full coverage.
                    let whole_line = (offset == 0 || normalized.as_bytes()[offset - 1] == b'\n')
                        && (offset + snippet.len() == normalized.len()
                            || normalized.as_bytes().get(offset + snippet.len()) == Some(&b'\n')
                            || snippet.ends_with('\n'));
                    fragments.push(FileFragment {
                        start_line,
                        end_line: if whole_line {
                            start_line + snippet.lines().count().saturating_sub(1)
                        } else {
                            start_line.saturating_sub(1)
                        },
                        text: snippet,
                    });
                }
            }
        }
        observations.push(FileObservation {
            resource,
            subject: prepared.subject.clone(),
            authorization_revision: prepared.revision.clone(),
            fingerprint: edits::version(after),
            total_lines,
            complete: full,
            fragments,
        });
    }
    Ok(observations)
}

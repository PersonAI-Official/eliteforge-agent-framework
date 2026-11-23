//! Context patch envelopes share confinement, staging and rollback with unified diffs.
use crate::{
    edits::{self, Change},
    filesystem::FileSystemRoot,
    text_patch::{FilePatch, apply_text_hunks, parse_text_patch},
};
use eliteforge_agent_core::{RunContext, ToolError};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path},
};

fn fail(message: impl ToString) -> ToolError {
    ToolError::Failed(message.to_string())
}

pub(crate) fn changes(
    filesystem: &FileSystemRoot,
    patch: &str,
    cwd: Option<&str>,
    context: &RunContext,
    prepared: &crate::coordinated_tools::PreparedFileEdit,
) -> Result<Vec<Change>, ToolError> {
    let operations = parse_text_patch(patch).map_err(fail)?;
    edits::ensure_active(context)?;
    let cwd = filesystem.resolve_directory(cwd)?;
    let resolve = |raw: &str| -> Result<(String, std::path::PathBuf), ToolError> {
        if raw.trim().is_empty()
            || Path::new(raw)
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(fail("patch paths must be nonempty and cannot contain .."));
        }
        let joined = cwd.path.join(raw);
        // Deleting or replacing a link must never silently edit its target.
        if fs::symlink_metadata(&joined).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(fail("patch file paths cannot be symbolic links"));
        }
        let path = filesystem.resolve(&filesystem.display(&cwd.root, &joined))?;
        Ok((filesystem.display(&path.root, &path.path), path.path))
    };
    let mut changes = BTreeMap::<String, Change>::new();
    let mut push = |path: String,
                    before: Option<Vec<u8>>,
                    after: Option<Vec<u8>>,
                    permissions: Option<fs::Permissions>|
     -> Result<(), ToolError> {
        if changes.keys().any(|existing| {
            Path::new(existing).starts_with(&path) || Path::new(&path).starts_with(existing)
        }) {
            return Err(fail("duplicate or overlapping file operation"));
        }
        if after
            .as_ref()
            .is_some_and(|bytes| bytes.len() > filesystem.max_edit_bytes() || bytes.contains(&0))
        {
            return Err(fail("patched file exceeds text file limits"));
        }
        changes.insert(
            path.clone(),
            Change {
                path,
                before,
                after,
                permissions,
            },
        );
        let total: usize = changes
            .values()
            .map(|change| {
                change.before.as_ref().map_or(0, Vec::len)
                    + change.after.as_ref().map_or(0, Vec::len)
            })
            .sum();
        if total > 32 * 1024 * 1024 {
            return Err(fail("patch working set exceeds 32 MiB"));
        }
        Ok(())
    };
    for operation in operations {
        edits::ensure_active(context)?;
        match operation {
            FilePatch::Add { path, content } => {
                let (display, path) = resolve(&path)?;
                if edits::read(&path, filesystem.max_edit_bytes())?.is_some() {
                    return Err(fail("file already exists"));
                }
                push(display, None, Some(content.into_bytes()), None)?;
            }
            FilePatch::Delete { path } => {
                let (display, path) = resolve(&path)?;
                let before = edits::read(&path, filesystem.max_edit_bytes())?
                    .ok_or_else(|| fail("file does not exist"))?;
                edits::text(&before)?;
                prepared.require_current(filesystem, &display, &before, true)?;
                push(
                    display,
                    Some(before),
                    None,
                    Some(fs::metadata(&path).map_err(fail)?.permissions()),
                )?;
            }
            FilePatch::Update {
                path,
                destination,
                hunks,
            } => {
                let (display, path) = resolve(&path)?;
                let before = edits::read(&path, filesystem.max_edit_bytes())?
                    .ok_or_else(|| fail("file does not exist"))?;
                prepared.require_current(filesystem, &display, &before, false)?;
                for hunk in &hunks {
                    if let Some(anchor) = &hunk.anchor {
                        prepared.require_seen(filesystem, &display, anchor)?;
                    }
                    if !hunk.before.is_empty() {
                        prepared.require_seen(filesystem, &display, &hunk.before.join("\n"))?;
                    }
                    if hunk.before.is_empty() && hunk.anchor.is_none() {
                        prepared.require_current(filesystem, &display, &before, true)?;
                    }
                }
                if destination.is_some() {
                    prepared.require_current(filesystem, &display, &before, false)?;
                    let observation = prepared.observation(filesystem, &display)?;
                    if observation.fingerprint != edits::version(&before) {
                        return Err(fail("move source changed; re-read the file"));
                    }
                }
                let after = apply_text_hunks(edits::text(&before)?, &hunks)
                    .map_err(fail)?
                    .into_bytes();
                let permissions = fs::metadata(&path).map_err(fail)?.permissions();
                if let Some(destination) = destination {
                    let (target_display, target) = resolve(&destination)?;
                    if path == target
                        || edits::read(&target, filesystem.max_edit_bytes())?.is_some()
                    {
                        return Err(fail(
                            "move destination must be a different, nonexistent file",
                        ));
                    }
                    push(display, Some(before), None, Some(permissions.clone()))?;
                    push(target_display, None, Some(after), Some(permissions))?;
                } else {
                    push(display, Some(before), Some(after), Some(permissions))?;
                }
            }
        }
    }
    Ok(changes.into_values().collect())
}

//! Stable text snapshots and staged file commits. Callers hold a coordination lease.
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

use eliteforge_agent_core::{RunContext, ToolError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::filesystem::FileSystemRoot;

pub(crate) fn version(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, ToolError> {
    match read_once(path, limit) {
        Err(ToolError::Failed(message)) if message.starts_with("file changed while reading") => {
            read_once(path, limit)
        }
        result => result,
    }
}
fn read_once(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, ToolError> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed(format!("read {}: {error}", path.display()))),
    };
    let before = file.metadata().map_err(io_error)?;
    if !before.is_file() {
        return Err(failed("only regular text files are supported"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > limit {
        return Err(failed("file exceeds max_edit_bytes"));
    }
    let after = fs::metadata(path).map_err(io_error)?;
    if !same_metadata(&before, &after) || before.len() != bytes.len() as u64 {
        return Err(failed("file changed while reading; re-read the file"));
    }
    Ok(Some(bytes))
}

fn same_metadata(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.size() == b.size()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
            && a.modified().ok() == b.modified().ok()
            && a.created().ok() == b.created().ok()
    }
}
pub(crate) fn text(bytes: &[u8]) -> Result<&str, ToolError> {
    let text = std::str::from_utf8(bytes).map_err(|_| failed("file is not UTF-8 text"))?;
    if text.contains('\0') {
        return Err(failed("binary files are not supported"));
    }
    Ok(text)
}

pub(crate) struct Change {
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    pub permissions: Option<fs::Permissions>,
}

pub(crate) struct CreatedDirectories(Vec<std::path::PathBuf>);
impl Drop for CreatedDirectories {
    fn drop(&mut self) {
        // Remove only empty directories made by this call. Successful edits and
        // external files keep their parent directories intact.
        for path in self.0.iter().rev() {
            let _ = fs::remove_dir(path);
        }
    }
}

pub(crate) fn prepare_parents(
    filesystem: &FileSystemRoot,
    changes: &[Change],
) -> Result<CreatedDirectories, ToolError> {
    let mut created = CreatedDirectories(Vec::new());
    for change in changes.iter().filter(|c| c.after.is_some()) {
        let resolved = filesystem.resolve(&change.path)?;
        let parent = resolved
            .path
            .parent()
            .ok_or_else(|| failed("missing parent directory"))?;
        let relative = parent
            .strip_prefix(&resolved.root)
            .map_err(|_| failed("parent outside root"))?;
        let mut current = resolved.root.clone();
        for component in relative.components() {
            current.push(component);
            let validated = filesystem.resolve(&filesystem.display(&resolved.root, &current))?;
            match fs::create_dir(&validated.path) {
                Ok(()) => created.0.push(validated.path),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && validated.path.is_dir() => {}
                Err(error) => return Err(io_error(error)),
            }
        }
    }
    Ok(created)
}

pub(crate) fn preview(change: &Change) -> Result<Value, ToolError> {
    let before = text(change.before.as_deref().unwrap_or_default())?;
    let after = text(change.after.as_deref().unwrap_or_default())?;
    let mut diff = diffy::create_patch(before, after).to_string();
    let truncated = diff.len() > 16384;
    if truncated {
        let mut end = 16384;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
    }
    Ok(json!({"path":change.path,
        "previousVersion": change.before.as_deref().map(version).unwrap_or_else(|| "absent".into()),
        "version": change.after.as_deref().map(version).unwrap_or_else(|| "absent".into()),
        "diff":diff,"diffTruncated":truncated}))
}

/// Caller holds the coordination lease. Stage in each destination directory, then recheck all
/// versions. A later failure restores earlier changes while preserving file modes.
pub(crate) fn commit(
    filesystem: &FileSystemRoot,
    changes: &[Change],
    context: Option<&RunContext>,
) -> Result<(), ToolError> {
    commit_with(filesystem, changes, context, |_| {})
}

fn commit_with(
    filesystem: &FileSystemRoot,
    changes: &[Change],
    context: Option<&RunContext>,
    mut before_write: impl FnMut(usize),
) -> Result<(), ToolError> {
    let lease = context.and_then(|c| {
        c.extensions
            .get::<crate::coordinated_tools::ActiveFileLease>()
    });
    let validate_lease = || -> Result<(), ToolError> {
        if let Some(lease) = &lease {
            tokio::runtime::Handle::current().block_on(lease.0.validate())?;
        }
        Ok(())
    };
    validate_lease()?;
    let mut staged = Vec::new();
    let mut permissions = Vec::new();
    for change in changes {
        if let Some(context) = context {
            ensure_active(context)?;
        }
        let resolved = filesystem.resolve(&change.path)?;
        let mode = fs::metadata(&resolved.path).ok().map(|m| m.permissions());
        permissions.push(mode.clone());
        staged.push(match &change.after {
            Some(bytes) => Some(stage(
                &resolved.path,
                bytes,
                change.permissions.clone().or(mode),
            )?),
            None => None,
        });
    }
    for change in changes {
        verify_unchanged(filesystem, change, &change.before)?;
    }
    for (index, change) in changes.iter().enumerate() {
        before_write(index);
        let result = (|| {
            if let Some(context) = context {
                ensure_active(context)?;
            }
            validate_lease()?;
            verify_unchanged(filesystem, change, &change.before)?;
            let resolved = filesystem.resolve(&change.path)?;
            match staged[index].take() {
                Some(file) if change.before.is_none() => file
                    .persist_noclobber(&resolved.path)
                    .map(|_| ())
                    .map_err(|e| io_error(e.error)),
                Some(file) => file
                    .persist(&resolved.path)
                    .map(|_| ())
                    .map_err(|e| io_error(e.error)),
                None => fs::remove_file(&resolved.path).map_err(io_error),
            }
        })();
        if let Err(error) = result {
            let mut failures = Vec::new();
            for undo_index in (0..index).rev() {
                let undo = &changes[undo_index];
                let result = (|| {
                    validate_lease()?;
                    verify_unchanged(filesystem, undo, &undo.after)?;
                    let resolved = filesystem.resolve(&undo.path)?;
                    match &undo.before {
                        Some(bytes) => {
                            stage(&resolved.path, bytes, permissions[undo_index].clone())?
                                .persist(&resolved.path)
                                .map(|_| ())
                                .map_err(|e| io_error(e.error))
                        }
                        None => fs::remove_file(&resolved.path).map_err(io_error),
                    }
                })();
                if let Err(error) = result {
                    failures.push(format!("{}: {error}", undo.path));
                }
            }
            if failures.is_empty() {
                return Err(failed(format!(
                    "edit failed; earlier file changes rolled back: {error}"
                )));
            }
            return Err(ToolError::Indeterminate(format!(
                "edit failed: {error}; rollback incomplete: {}",
                failures.join("; ")
            )));
        }
    }
    Ok(())
}

fn verify_unchanged(
    filesystem: &FileSystemRoot,
    change: &Change,
    expected: &Option<Vec<u8>>,
) -> Result<(), ToolError> {
    let path = filesystem.resolve(&change.path)?.path;
    let actual = read(&path, filesystem.max_edit_bytes())?;
    if actual != *expected {
        return Err(failed(format!(
            "version conflict during commit: {}",
            change.path
        )));
    }
    Ok(())
}

fn stage(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<NamedTempFile, ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| failed("file must have a parent directory"))?;
    let mut file = NamedTempFile::new_in(parent).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    if let Some(permissions) = permissions {
        file.as_file()
            .set_permissions(permissions)
            .map_err(io_error)?;
    }
    file.as_file().sync_all().map_err(io_error)?;
    Ok(file)
}

pub(crate) fn ensure_active(context: &RunContext) -> Result<(), ToolError> {
    if context.cancellation.is_cancelled() {
        Err(failed("file operation cancelled"))
    } else {
        Ok(())
    }
}
fn failed(message: impl Into<String>) -> ToolError {
    ToolError::Failed(message.into())
}
fn io_error(error: std::io::Error) -> ToolError {
    failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileSystemToolsConfig;

    #[test]
    fn failure_after_first_write_restores_old_content_and_reports_conflict() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"old").unwrap();
        fs::write(root.path().join("b"), b"old").unwrap();
        let filesystem = FileSystemRoot::new(FileSystemToolsConfig::new(root.path()));
        let changes = ["a", "b"].map(|path| Change {
            path: path.into(),
            before: Some(b"old".to_vec()),
            after: Some(b"new".to_vec()),
            permissions: None,
        });
        let error = commit_with(&filesystem, &changes, None, |index| {
            if index == 1 {
                fs::write(root.path().join("b"), b"external writer").unwrap();
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("rolled back"));
        assert_eq!(fs::read(root.path().join("a")).unwrap(), b"old");
        assert_eq!(fs::read(root.path().join("b")).unwrap(), b"external writer");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn rollback_refuses_to_overwrite_a_later_external_edit() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            fs::write(root.path().join(name), b"old").unwrap();
        }
        let filesystem = FileSystemRoot::new(FileSystemToolsConfig::new(root.path()));
        let changes = ["a", "b"].map(|path| Change {
            path: path.into(),
            before: Some(b"old".to_vec()),
            after: Some(b"new".to_vec()),
            permissions: None,
        });
        let error = commit_with(&filesystem, &changes, None, |index| {
            if index == 1 {
                for name in ["a", "b"] {
                    fs::write(root.path().join(name), b"external writer").unwrap();
                }
            }
        })
        .unwrap_err();
        assert!(matches!(error, ToolError::Indeterminate(_)));
        for name in ["a", "b"] {
            assert_eq!(
                fs::read(root.path().join(name)).unwrap(),
                b"external writer"
            );
        }
    }
}

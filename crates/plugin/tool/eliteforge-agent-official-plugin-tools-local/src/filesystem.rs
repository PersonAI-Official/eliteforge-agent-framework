use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use eliteforge_agent_core::ToolError;

const DEFAULT_MAX_READ_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_SEARCH_FILE_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_RESULTS: usize = 200;
const DEFAULT_MAX_DEPTH: usize = 8;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_ASYNC_AFTER: Duration = Duration::from_secs(10);

/// Bounds and directory authority for file and search tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSystemToolsConfig {
    root: PathBuf,
    authorized_roots: Option<Vec<PathBuf>>,
    follow_authorized_symlinks: bool,
    max_read_bytes: usize,
    max_search_file_bytes: usize,
    max_results: usize,
    max_depth: usize,
    max_edit_bytes: usize,
    coordination_roots: Vec<(PathBuf, String, String)>,
}

impl FileSystemToolsConfig {
    /// `root` must exist and be a directory when a tool is called.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            authorized_roots: None,
            follow_authorized_symlinks: false,
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
            max_search_file_bytes: DEFAULT_MAX_SEARCH_FILE_BYTES,
            max_results: DEFAULT_MAX_RESULTS,
            max_depth: DEFAULT_MAX_DEPTH,
            max_edit_bytes: 8 * 1024 * 1024,
            coordination_roots: Vec::new(),
        }
    }

    /// Use the host-selected working directory for relative paths and allow
    /// absolute paths inside explicitly authorized directories. An empty list
    /// denies access. All directories must exist; there is no cwd fallback.
    pub fn with_authorized_roots(
        mut self,
        roots: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Self {
        self.authorized_roots = Some(roots.into_iter().map(Into::into).collect());
        self
    }

    /// Allow symlink targets only when their resolved existing ancestor remains
    /// inside a host-authorized root. Default false rejects every symlink.
    pub fn with_authorized_symlinks(mut self, allowed: bool) -> Self {
        self.follow_authorized_symlinks = allowed;
        self
    }

    pub fn with_max_read_bytes(mut self, value: usize) -> Self {
        self.max_read_bytes = value.max(1);
        self
    }

    pub fn with_max_search_file_bytes(mut self, value: usize) -> Self {
        self.max_search_file_bytes = value.max(1);
        self
    }

    pub fn with_max_results(mut self, value: usize) -> Self {
        self.max_results = value.max(1);
        self
    }

    pub fn with_max_depth(mut self, value: usize) -> Self {
        self.max_depth = value;
        self
    }

    /// Assign a stable host-owned identity to a physical workspace root. Map
    /// every shared mount explicitly before using a distributed backend.
    pub fn with_coordination_root(
        mut self,
        root: impl Into<PathBuf>,
        storage_namespace: impl Into<String>,
        workspace_id: impl Into<String>,
    ) -> Self {
        self.coordination_roots
            .push((root.into(), storage_namespace.into(), workspace_id.into()));
        self
    }
    /// Maximum size of a text snapshot used for reading, versioning and editing.
    pub fn with_max_edit_bytes(mut self, value: usize) -> Self {
        self.max_edit_bytes = value.max(1);
        self
    }
}

/// Command execution settings for the system-command capability pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemCommandConfig {
    directory_scope: Option<FileSystemToolsConfig>,
    shell: Option<PathBuf>,
    timeout: Duration,
    async_after: Duration,
    max_output_bytes: usize,
}

impl SystemCommandConfig {
    /// Allow every executable. Selecting this capability pack remains explicit.
    pub fn new() -> Self {
        Self {
            directory_scope: None,
            shell: None,
            timeout: DEFAULT_COMMAND_TIMEOUT,
            async_after: DEFAULT_ASYNC_AFTER,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Set the maximum command lifetime. The default is 30 minutes.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Return a task ID instead of waiting once a command reaches this duration.
    /// The default is 10 seconds; the command keeps running up to [`Self::with_timeout`].
    pub fn with_async_after(mut self, duration: Duration) -> Self {
        self.async_after = duration;
        self
    }

    pub fn with_max_output_bytes(mut self, value: usize) -> Self {
        self.max_output_bytes = value.max(1);
        self
    }

    /// Resolve omitted/relative cwd against the host-selected directory and
    /// validate absolute cwd against its grants. This does not sandbox commands.
    pub fn with_directory_scope(mut self, scope: FileSystemToolsConfig) -> Self {
        self.directory_scope = Some(scope);
        self
    }

    /// Select the shell used for the model-facing complete command string.
    /// Explicit internal `args`, including [], always executes the literal
    /// program and argument vector. The shell is host-selected, never selected
    /// by tool arguments.
    pub fn with_shell(mut self, shell: impl Into<PathBuf>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    pub(crate) fn configured_shell(&self) -> Option<&std::path::Path> {
        self.shell.as_deref()
    }

    pub(crate) fn invocation(
        &self,
        command: String,
        args: Option<Vec<String>>,
        interactive: bool,
    ) -> Result<(String, Vec<String>), ToolError> {
        // Explicit argv stays a direct program invocation. The model-facing
        // command string always runs through the host shell, using the
        // host-selected shell or the platform default.
        if let Some(args) = args {
            return Ok((command, args));
        }
        let shell = self
            .shell
            .clone()
            .unwrap_or_else(crate::command_shell::default_shell_program);
        if !shell.is_absolute() || !shell.is_file() {
            return Err(ToolError::Failed(
                "configured shell must be an existing absolute executable path".into(),
            ));
        }
        let shell = shell
            .to_str()
            .ok_or_else(|| ToolError::Failed("shell path is not UTF-8".into()))?;
        Ok((
            shell.into(),
            crate::command_shell::arguments(
                std::path::Path::new(shell),
                command,
                true,
                interactive,
            ),
        ))
    }

    pub(crate) fn interactive_shell(&self) -> Result<PathBuf, ToolError> {
        let shell = self.shell.as_ref().ok_or_else(|| {
            ToolError::Failed("interactive terminal requires a host-configured shell".into())
        })?;
        if !shell.is_absolute() || !shell.is_file() {
            return Err(ToolError::Failed(
                "configured shell must be an existing absolute executable path".into(),
            ));
        }
        Ok(shell.clone())
    }
    /// Terminal commands use the configured shell without reading login files.
    pub(crate) fn terminal_invocation(
        &self,
        command: String,
        args: Option<Vec<String>>,
    ) -> Result<(String, Vec<String>), ToolError> {
        if command.is_empty() {
            return Err(ToolError::Failed("command is required".into()));
        }
        let (program, args) = match args {
            Some(args) => (command, args),
            None => {
                let shell = self.interactive_shell()?;
                (
                    shell.to_string_lossy().into_owned(),
                    crate::command_shell::arguments(&shell, command, false, true),
                )
            }
        };
        Ok((program, args))
    }

    pub(crate) fn resolve_cwd(&self, cwd: Option<&str>) -> Result<Option<PathBuf>, ToolError> {
        match &self.directory_scope {
            Some(scope) => Ok(Some(
                FileSystemRoot::new(scope.clone())
                    .resolve_directory(cwd)?
                    .path,
            )),
            None => Ok(cwd.map(PathBuf::from)),
        }
    }

    pub(crate) fn authorize_existing_cwd(&self, cwd: Option<&str>) -> Result<(), ToolError> {
        if self.directory_scope.is_some() {
            let cwd = cwd.ok_or_else(|| {
                ToolError::Failed("command has no authorized working directory".into())
            })?;
            self.resolve_cwd(Some(cwd))?;
        }
        Ok(())
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn async_after(&self) -> Duration {
        self.async_after
    }

    pub(crate) fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

impl Default for SystemCommandConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FileSystemRoot {
    config: FileSystemToolsConfig,
    pub(crate) coordination: crate::coordination::CoordinationSelection,
    pub(crate) searches: std::sync::Arc<std::sync::Mutex<crate::search::SearchCache>>,
}

pub(crate) struct ResolvedPath {
    pub(crate) root: PathBuf,
    pub(crate) path: PathBuf,
}

impl FileSystemRoot {
    pub(crate) fn new(config: FileSystemToolsConfig) -> Self {
        Self {
            config,
            coordination: Default::default(),
            searches: Default::default(),
        }
    }

    pub(crate) fn with_coordination(
        mut self,
        selection: crate::coordination::CoordinationSelection,
    ) -> Self {
        self.coordination = selection;
        self
    }
    pub(crate) fn authorization_revision(&self) -> String {
        use sha2::Digest;
        format!(
            "{:x}",
            sha2::Sha256::digest(format!("{:?}", self.config).as_bytes())
        )
    }
    pub(crate) fn resource(
        &self,
        raw: &str,
    ) -> Result<crate::coordination::FileResource, ToolError> {
        let resolved = self.resolve(raw)?;
        let original = resolved.path;
        let mut existing = original.as_path();
        while fs::symlink_metadata(existing).is_err() {
            existing = existing
                .parent()
                .ok_or_else(|| ToolError::Failed("path has no existing ancestor".into()))?;
        }
        let physical = fs::canonicalize(existing).map_err(|e| ToolError::Failed(e.to_string()))?;
        let suffix = original
            .strip_prefix(existing)
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let path = if suffix.as_os_str().is_empty() {
            physical
        } else {
            physical.join(suffix)
        };
        let mapping = self
            .config
            .coordination_roots
            .iter()
            .filter_map(|(root, namespace, workspace)| {
                let root = fs::canonicalize(root).ok()?;
                path.strip_prefix(&root).ok().map(|relative| {
                    (
                        root.components().count(),
                        namespace.clone(),
                        workspace.clone(),
                        relative.to_path_buf(),
                    )
                })
            })
            .max_by_key(|m| m.0);
        let (storage_namespace, workspace_id, relative) =
            if let Some((_, namespace, workspace, relative)) = mapping {
                (namespace, workspace, relative)
            } else {
                let backend = self.coordination.backend()?;
                if !matches!(
                    backend.capabilities().scope,
                    crate::coordination::FileCoordinationScope::Process
                ) {
                    return Err(ToolError::Failed(
                        "distributed file coordination requires an explicit workspace mapping"
                            .into(),
                    ));
                }
                let physical_root = self
                    .config
                    .authorized_roots
                    .as_ref()
                    .and_then(|roots| {
                        roots
                            .iter()
                            .filter_map(|r| fs::canonicalize(r).ok())
                            .filter(|r| path.starts_with(r))
                            .max_by_key(|r| r.components().count())
                    })
                    .unwrap_or_else(|| resolved.root.clone());
                (
                    "local".into(),
                    physical_root.to_string_lossy().into_owned(),
                    path.strip_prefix(&physical_root)
                        .map_err(|e| ToolError::Failed(e.to_string()))?
                        .to_path_buf(),
                )
            };
        if storage_namespace.is_empty() || workspace_id.is_empty() {
            return Err(ToolError::Failed("empty file coordination identity".into()));
        }
        Ok(crate::coordination::FileResource {
            storage_namespace,
            workspace_id,
            path: relative.to_string_lossy().replace('\\', "/"),
            physical_alias: path.to_string_lossy().into_owned(),
        })
    }
    pub(crate) fn fresh(&self) -> Self {
        Self::new(self.config.clone())
    }

    pub(crate) fn resolve(&self, raw: &str) -> Result<ResolvedPath, ToolError> {
        let root = fs::canonicalize(&self.config.root).map_err(|error| {
            ToolError::Failed(format!(
                "filesystem root `{}` is unavailable: {error}",
                self.config.root.display()
            ))
        })?;
        if !root.is_dir() {
            return Err(ToolError::Failed(format!(
                "filesystem root `{}` is not a directory",
                root.display()
            )));
        }

        let (root, path) = if let Some(authorized) = &self.config.authorized_roots {
            let mut roots = Vec::with_capacity(authorized.len());
            let mut aliases = Vec::with_capacity(authorized.len());
            for configured in authorized {
                if !configured.is_absolute() {
                    return Err(ToolError::Failed(
                        "authorized roots must be absolute".into(),
                    ));
                }
                let directory = fs::canonicalize(configured).map_err(|error| {
                    ToolError::Failed(format!("authorized root unavailable: {error}"))
                })?;
                if !directory.is_dir() {
                    return Err(ToolError::Failed(
                        "authorized root is not a directory".into(),
                    ));
                }
                aliases.push((configured, directory.clone()));
                roots.push(directory);
            }
            if !self.config.root.is_absolute() || !roots.iter().any(|r| root.starts_with(r)) {
                return Err(ToolError::Failed(
                    "working directory is outside authorized roots".into(),
                ));
            }
            let raw = raw.trim();
            if raw.is_empty()
                || Path::new(raw)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir))
            {
                return Err(ToolError::Failed(
                    "empty paths and parent traversal are not allowed".into(),
                ));
            }
            let path = if Path::new(raw).is_absolute() {
                // Normalize only the host-authorized prefix (e.g. /var on
                // macOS). Never canonicalize a model-supplied suffix before
                // the symlink checks, or a symlink escape could be concealed.
                let raw_path = Path::new(raw);
                aliases
                    .iter()
                    .filter_map(|(alias, canonical)| {
                        raw_path
                            .strip_prefix(alias)
                            .ok()
                            .map(|suffix| (alias.components().count(), canonical.join(suffix)))
                    })
                    .max_by_key(|(length, _)| *length)
                    .map(|(_, path)| path)
                    .unwrap_or_else(|| raw_path.to_path_buf())
            } else {
                root.join(relative_path(raw)?)
            };
            // Prefer the narrowest authorized root for nested directory grants.
            let selected = roots
                .into_iter()
                .filter(|r| path.starts_with(r))
                .max_by_key(|r| r.components().count())
                .ok_or_else(|| ToolError::Failed("path is outside authorized roots".into()))?;
            (selected, path)
        } else {
            let relative = relative_path(raw)?;
            (
                root.clone(),
                root.join(relative.components().collect::<PathBuf>()),
            )
        };
        if self.config.follow_authorized_symlinks {
            let mut existing = path.as_path();
            while fs::symlink_metadata(existing).is_err() {
                existing = existing
                    .parent()
                    .ok_or_else(|| ToolError::Failed("path has no existing ancestor".into()))?;
            }
            let resolved = fs::canonicalize(existing).map_err(|error| {
                ToolError::Failed(format!("symlink target cannot be resolved: {error}"))
            })?;
            let allowed = match &self.config.authorized_roots {
                Some(roots) => roots
                    .iter()
                    .map(fs::canonicalize)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| ToolError::Failed(error.to_string()))?,
                None => vec![root.clone()],
            };
            if !allowed.iter().any(|root| resolved.starts_with(root)) {
                return Err(ToolError::Failed(
                    "symlink target is outside authorized roots".into(),
                ));
            }
            return Ok(ResolvedPath { root, path });
        }
        ensure_no_symlinks(&root, &path)?;
        ensure_existing_ancestor_is_within(&root, &path)?;
        if path.exists() {
            let resolved = fs::canonicalize(&path).map_err(|error| {
                ToolError::Failed(format!("resolve `{}` failed: {error}", path.display()))
            })?;
            ensure_within(&root, &resolved)?;
        }
        Ok(ResolvedPath { root, path })
    }

    pub(crate) fn resolve_directory(&self, raw: Option<&str>) -> Result<ResolvedPath, ToolError> {
        let resolved = self.resolve(raw.unwrap_or("."))?;
        let metadata = fs::metadata(&resolved.path).map_err(|error| {
            ToolError::Failed(format!(
                "filesystem directory `{}` is unavailable: {error}",
                resolved.path.display()
            ))
        })?;
        if !metadata.is_dir() {
            return Err(ToolError::Failed(format!(
                "`{}` is not a directory",
                resolved.path.display()
            )));
        }
        Ok(resolved)
    }

    pub(crate) fn max_read_bytes(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.config.max_read_bytes)
            .clamp(1, self.config.max_read_bytes)
    }

    pub(crate) fn max_search_file_bytes(&self) -> usize {
        self.config.max_search_file_bytes
    }

    pub(crate) fn max_edit_bytes(&self) -> usize {
        self.config.max_edit_bytes
    }

    pub(crate) fn max_results(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.config.max_results)
            .clamp(1, self.config.max_results)
    }

    pub(crate) fn max_depth(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.config.max_depth)
            .min(self.config.max_depth)
    }

    pub(crate) fn display(&self, root: &Path, path: &Path) -> String {
        if self.config.authorized_roots.is_some() {
            return path.to_string_lossy().replace('\\', "/");
        }
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

fn relative_path(raw: &str) -> Result<&Path, ToolError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ToolError::Failed("path is required".into()));
    }
    let path = Path::new(raw);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolError::Failed(
            "path must be relative to the configured filesystem root".into(),
        ));
    }
    Ok(path)
}

fn ensure_existing_ancestor_is_within(root: &Path, candidate: &Path) -> Result<(), ToolError> {
    let mut ancestor = candidate;
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            ToolError::Failed("path has no ancestor inside the filesystem root".into())
        })?;
    }
    let resolved = fs::canonicalize(ancestor).map_err(|error| {
        ToolError::Failed(format!("resolve `{}` failed: {error}", ancestor.display()))
    })?;
    ensure_within(root, &resolved)
}

fn ensure_no_symlinks(root: &Path, candidate: &Path) -> Result<(), ToolError> {
    let relative = candidate
        .strip_prefix(root)
        .map_err(|_| ToolError::Failed("path escapes the configured filesystem root".into()))?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(ToolError::Failed(format!(
                    "inspect `{}` failed: {error}",
                    current.display()
                )));
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(ToolError::Failed(
                "symbolic links are not allowed in filesystem tool paths".into(),
            ));
        }
    }
    Ok(())
}

fn ensure_within(root: &Path, candidate: &Path) -> Result<(), ToolError> {
    if candidate.starts_with(root) {
        Ok(())
    } else {
        Err(ToolError::Failed(
            "path escapes the configured filesystem root".into(),
        ))
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    #[test]
    fn multi_root_paths_keep_explicit_authority_and_working_directory() {
        let base = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(base.path()).unwrap();
        let first = base.join("first");
        let second = base.join("second");
        let other = base.join("second-sibling");
        for p in [&first, &second, &other] {
            fs::create_dir(p).unwrap();
        }
        let root = FileSystemRoot::new(
            FileSystemToolsConfig::new(&first).with_authorized_roots([&first, &second]),
        );
        assert_eq!(root.resolve("new.txt").unwrap().path, first.join("new.txt"));
        let target = second.join("new.txt");
        let resolved = root.resolve(target.to_str().unwrap()).unwrap();
        assert_eq!(resolved.path, target);
        assert_eq!(
            root.display(&resolved.root, &resolved.path),
            target.to_str().unwrap()
        );
        assert!(
            root.resolve(other.join("new.txt").to_str().unwrap())
                .is_err()
        );
        assert!(root.resolve("../second/new.txt").is_err());
        let denied = FileSystemRoot::new(
            FileSystemToolsConfig::new(&first).with_authorized_roots(Vec::<PathBuf>::new()),
        );
        assert!(denied.resolve("new.txt").is_err());
        fs::remove_dir(&first).unwrap();
        assert!(root.resolve(target.to_str().unwrap()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn multi_root_cannot_follow_symlinks_between_granted_directories() {
        let base = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(base.path()).unwrap();
        let first = base.join("first");
        let second = base.join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&second, first.join("link")).unwrap();
        let root = FileSystemRoot::new(
            FileSystemToolsConfig::new(&first).with_authorized_roots([&first, &second]),
        );
        assert!(root.resolve("link/new.txt").is_err());
        let fixed = FileSystemRoot::new(FileSystemToolsConfig::new(&first));
        assert!(
            fixed
                .resolve(first.join("new.txt").to_str().unwrap())
                .is_err()
        );
    }
}

#[cfg(test)]
mod terminal_command_tests {
    use super::*;
    fn resolve_command_invocation(
        command: &str,
        args: Option<&[String]>,
        _: &Path,
    ) -> Result<(String, Vec<String>), ToolError> {
        SystemCommandConfig::new()
            .with_shell("/bin/sh")
            .terminal_invocation(command.into(), args.map(|values| values.to_vec()))
    }
    #[cfg(unix)]
    #[test]
    fn complete_command_line_uses_the_users_preferred_noninteractive_shell() {
        let cwd = Path::new("/tmp");
        let shell = PathBuf::from("/bin/sh").to_string_lossy().into_owned();
        assert_eq!(
            resolve_command_invocation("npm run dev", None, cwd).unwrap(),
            (shell.clone(), vec!["-c".into(), "npm run dev".into()])
        );
        assert_eq!(
            resolve_command_invocation("bash -c \"cd front && npm run dev\"", None, cwd).unwrap(),
            (
                shell.clone(),
                vec!["-c".into(), "bash -c \"cd front && npm run dev\"".into()]
            )
        );
        assert_eq!(
            resolve_command_invocation("cd front && npm run dev", None, cwd).unwrap(),
            (shell, vec!["-c".into(), "cd front && npm run dev".into()])
        );
    }

    #[cfg(windows)]
    #[test]
    fn full_command_line_uses_cmd_on_windows() {
        assert_eq!(
            resolve_command_invocation("npm run dev", None, Path::new(".")).unwrap(),
            ("cmd".into(), vec!["/C".into(), "npm run dev".into()])
        );
    }

    #[test]
    fn explicit_args_and_existing_executable_paths_stay_literal() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("program with spaces");
        fs::write(&executable, "").unwrap();
        let path = executable.to_string_lossy();
        assert_eq!(
            resolve_command_invocation(&path, Some(&[]), temporary.path()).unwrap(),
            (path.to_string(), vec![])
        );
        assert_eq!(
            resolve_command_invocation("echo", Some(&["a b".into()]), temporary.path()).unwrap(),
            ("echo".into(), vec!["a b".into()])
        );
        assert_eq!(
            resolve_command_invocation("echo", Some(&[]), temporary.path()).unwrap(),
            ("echo".into(), Vec::new())
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_syntax_is_passed_to_the_selected_shell_for_interpretation() {
        let shell = PathBuf::from("/bin/sh").to_string_lossy().into_owned();
        assert_eq!(
            resolve_command_invocation("bash -c 'unterminated", None, Path::new("/tmp")).unwrap(),
            (shell, vec!["-c".into(), "bash -c 'unterminated".into()])
        );
    }
}

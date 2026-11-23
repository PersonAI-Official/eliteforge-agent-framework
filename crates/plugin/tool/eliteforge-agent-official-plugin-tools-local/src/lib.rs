//! In-process local tool implementations, with command tasks owned by the Agent.
//! Filesystem paths are confined to the configured root. Commands use literal
//! arguments and optional executable allowlists; this is not a process sandbox.

/// Tool extension implementation description owned by this plugin package.
pub const TOOL_IMPLEMENTATION_DESCRIPTION: &str =
    "Provides workspace-scoped filesystem, command and shared PTY capabilities.";

pub mod capture;
mod output_capture;
pub use output_capture::{
    CommandOutputCaptureProvider, CommandOutputCaptureService, CommandOutputFile,
    CommandOutputWriter,
};
mod coordinated_tools;
pub mod coordination;
mod edits;
mod filesystem;
mod patch_envelope;
pub mod process_tree;
pub mod pty;
mod pty_process;
pub mod pty_tools;
mod search;
mod sudo;
mod tasks;
pub use pty_process::PtyCommand;
pub use sudo::{SudoPassword, SudoPasswordProvider, SudoPasswordProviderService};
pub mod text_patch;
mod tools;

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, Plugin, PluginContext, PluginError, RunContext, ToolLease, ToolProvider,
    ToolProviderPlugin,
};

pub use filesystem::{FileSystemToolsConfig, SystemCommandConfig};
mod command_shell;
pub use command_shell::default_shell_program;

/// Host authority resolved once per run lease, never supplied by model arguments.
/// Implementations must validate the run's tenant/session permissions before
/// returning a directory grant. Returning an error grants no filesystem tools.
#[async_trait]
pub trait FileSystemScopeProvider: Send + Sync {
    async fn resolve(&self, run: &RunContext) -> Result<FileSystemToolsConfig, AgentError>;
}

#[derive(Clone)]
enum FileSystemScope {
    Fixed(filesystem::FileSystemRoot),
    Host(std::sync::Arc<dyn FileSystemScopeProvider>),
}

impl FileSystemScope {
    async fn lease(&self, run: &RunContext) -> Result<filesystem::FileSystemRoot, AgentError> {
        match self {
            Self::Fixed(root) => Ok(root.clone()),
            Self::Host(provider) => Ok(filesystem::FileSystemRoot::new(
                provider.resolve(run).await?,
            )),
        }
    }
    fn fresh(&self) -> Self {
        match self {
            Self::Fixed(root) => Self::Fixed(root.fresh()),
            Self::Host(provider) => Self::Host(provider.clone()),
        }
    }
}

/// Inert catalog for host policy/configuration UIs. Uses the same descriptors
/// as execution, without opening files, starting commands or installing a plugin.
/// The catalog grants no authority; hosts must still select the corresponding packs.
pub fn local_tool_catalog() -> Vec<eliteforge_agent_core::ToolDescriptor> {
    local_tool_catalog_metadata()
        .into_iter()
        .map(|entry| entry.descriptor)
        .collect()
}

/// Complete registration inventory, independent of the current run's grant.
pub fn local_tool_catalog_metadata() -> Vec<eliteforge_agent_core::ToolCatalogEntry> {
    let root = filesystem::FileSystemRoot::new(FileSystemToolsConfig::new("."));
    tools::read_only_bindings(root.clone())
        .into_iter()
        .chain(tools::write_bindings(root))
        .chain(tools::system_command_bindings(
            SystemCommandConfig::new(),
            tasks::CommandTaskManager::default(),
        ))
        .map(|binding| binding.with_category(local_tool_category()).catalog_entry())
        .chain(
            pty_tools::terminal_tool_catalog()
                .into_iter()
                .map(|descriptor| eliteforge_agent_core::ToolCatalogEntry {
                    descriptor,
                    domain_code: Some("terminal".into()),
                    category: Some(local_tool_category()),
                }),
        )
        .collect()
}

fn local_tool_category() -> eliteforge_agent_core::ToolCategory {
    eliteforge_agent_core::ToolCategory::localized(
        "system.command",
        "系统命令工具",
        "System command tools",
    )
}

/// Shared path confinement for optional local readers. Refuses absolute paths,
/// traversal and symlinks, and bounds the bytes read from a regular file.
#[derive(Clone)]
pub struct ConfinedFileReader(filesystem::FileSystemRoot);

pub struct ConfinedFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

impl ConfinedFileReader {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self(filesystem::FileSystemRoot::new(FileSystemToolsConfig::new(
            root,
        )))
    }

    pub fn from_config(config: FileSystemToolsConfig) -> Self {
        Self(filesystem::FileSystemRoot::new(config))
    }

    pub fn read(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<ConfinedFile, eliteforge_agent_core::ToolError> {
        let resolved = self.0.resolve(path)?;
        let bytes = edits::read(&resolved.path, max_bytes)?.ok_or_else(|| {
            eliteforge_agent_core::ToolError::Failed("file does not exist".into())
        })?;
        Ok(ConfinedFile {
            path: self.0.display(&resolved.root, &resolved.path),
            bytes,
        })
    }
}

/// Directory-confined file reading, listing and search.
#[derive(Clone)]
pub struct ReadOnlyFileSystemPlugin {
    filesystem: FileSystemScope,
    coordination: coordination::CoordinationSelection,
}

impl ReadOnlyFileSystemPlugin {
    pub fn new(config: FileSystemToolsConfig) -> Self {
        Self {
            filesystem: FileSystemScope::Fixed(filesystem::FileSystemRoot::new(config)),
            coordination: Default::default(),
        }
    }

    pub fn scoped(provider: std::sync::Arc<dyn FileSystemScopeProvider>) -> Self {
        Self {
            filesystem: FileSystemScope::Host(provider),
            coordination: Default::default(),
        }
    }
}

#[async_trait]
impl ToolProvider for ReadOnlyFileSystemPlugin {
    fn catalog_metadata(&self) -> Option<Vec<eliteforge_agent_core::ToolCatalogEntry>> {
        Some(
            local_tool_catalog_metadata()
                .into_iter()
                .filter(|entry| {
                    matches!(entry.domain_code.as_deref(), Some("fs" | "search"))
                        && entry.descriptor.operation_kind
                            == eliteforge_agent_core::ToolOperationKind::Query
                })
                .collect(),
        )
    }
    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(
            tools::read_only_bindings(
                self.filesystem
                    .lease(run)
                    .await?
                    .with_coordination(self.coordination.clone()),
            )
            .into_iter()
            .map(|tool| {
                tool.with_category(eliteforge_agent_core::ToolCategory::localized(
                    "system.command",
                    "系统命令工具",
                    "System command tools",
                ))
            })
            .collect(),
        ))
    }
}

#[async_trait]
impl Plugin for ReadOnlyFileSystemPlugin {
    fn id(&self) -> &str {
        "framework.filesystem.readonly.tools"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        ToolProviderPlugin::new(
            self.id(),
            Self {
                filesystem: self.filesystem.fresh(),
                coordination: self.coordination.clone(),
            },
        )
        .install(context)
        .await
    }
    async fn ready(
        &self,
        context: &mut eliteforge_agent_core::PluginReadyContext<'_>,
    ) -> Result<(), PluginError> {
        self.coordination.ready(context);
        Ok(())
    }
}

/// Directory-confined text writes and patches.
#[derive(Clone)]
pub struct WriteFileSystemPlugin {
    filesystem: FileSystemScope,
    coordination: coordination::CoordinationSelection,
}

impl WriteFileSystemPlugin {
    pub fn new(config: FileSystemToolsConfig) -> Self {
        Self {
            filesystem: FileSystemScope::Fixed(filesystem::FileSystemRoot::new(config)),
            coordination: Default::default(),
        }
    }

    pub fn scoped(provider: std::sync::Arc<dyn FileSystemScopeProvider>) -> Self {
        Self {
            filesystem: FileSystemScope::Host(provider),
            coordination: Default::default(),
        }
    }
}

#[async_trait]
impl ToolProvider for WriteFileSystemPlugin {
    fn catalog_metadata(&self) -> Option<Vec<eliteforge_agent_core::ToolCatalogEntry>> {
        Some(
            local_tool_catalog_metadata()
                .into_iter()
                .filter(|entry| {
                    entry.domain_code.as_deref() == Some("fs")
                        && entry.descriptor.operation_kind
                            == eliteforge_agent_core::ToolOperationKind::Command
                })
                .collect(),
        )
    }
    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(
            tools::write_bindings(
                self.filesystem
                    .lease(run)
                    .await?
                    .with_coordination(self.coordination.clone()),
            )
            .into_iter()
            .map(|tool| {
                tool.with_category(eliteforge_agent_core::ToolCategory::localized(
                    "system.command",
                    "系统命令工具",
                    "System command tools",
                ))
            })
            .collect(),
        ))
    }
}

#[async_trait]
impl Plugin for WriteFileSystemPlugin {
    fn id(&self) -> &str {
        "framework.filesystem.write.tools"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        ToolProviderPlugin::new(self.id(), self.clone())
            .install(context)
            .await
    }
    async fn ready(
        &self,
        context: &mut eliteforge_agent_core::PluginReadyContext<'_>,
    ) -> Result<(), PluginError> {
        self.coordination.ready(context);
        Ok(())
    }
}

/// Literal command execution, task output polling and task cancellation.
#[derive(Clone)]
pub struct SystemCommandPlugin {
    config: SystemCommandConfig,
}

impl SystemCommandPlugin {
    pub fn new(config: SystemCommandConfig) -> Self {
        Self { config }
    }
}

struct SystemCommandTools {
    config: SystemCommandConfig,
    tasks: tasks::CommandTaskManager,
}

#[async_trait]
impl ToolProvider for SystemCommandTools {
    fn catalog_metadata(&self) -> Option<Vec<eliteforge_agent_core::ToolCatalogEntry>> {
        Some(
            local_tool_catalog_metadata()
                .into_iter()
                .filter(|entry| entry.domain_code.as_deref() == Some("command"))
                .collect(),
        )
    }
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(
            tools::system_command_bindings(self.config.clone(), self.tasks.clone())
                .into_iter()
                .map(|tool| {
                    tool.with_category(eliteforge_agent_core::ToolCategory::localized(
                        "system.command",
                        "系统命令工具",
                        "System command tools",
                    ))
                })
                .collect(),
        ))
    }
}

#[async_trait]
impl Plugin for SystemCommandPlugin {
    fn id(&self) -> &str {
        "framework.system-command.tools"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let tasks = tasks::CommandTaskManager::default();
        let fallback_tasks = tasks.clone();
        context.on_cleanup(move || fallback_tasks.stop_all());
        let shutdown_tasks = tasks.clone();
        context.on_shutdown(move || async move { shutdown_tasks.shutdown().await });
        ToolProviderPlugin::new(
            self.id(),
            SystemCommandTools {
                config: self.config.clone(),
                tasks,
            },
        )
        .install(context)
        .await
    }
}

mod scope;
pub use scope::{
    LocalToolGrant, LocalToolScopeProvider, LocalToolScopeRevision, LocalToolScopeSnapshot,
    ScopedLocalToolsPlugin,
};

/// Product-owned resource authority. The official text reader owns slicing,
/// UTF-8 validation and output bounds; a host resolves its opaque references.
/// Implementations must check workspace/session access and bound reads by max_bytes.
#[async_trait]
pub trait FileReferenceReader: Send + Sync {
    async fn read(
        &self,
        reference: serde_json::Value,
        max_bytes: usize,
        run: &RunContext,
    ) -> Result<ReferencedFile, eliteforge_agent_core::ToolError>;
}
pub struct ReferencedFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub reference: serde_json::Value,
}
#[derive(Clone)]
pub struct FileReferenceReaderService(pub std::sync::Arc<dyn FileReferenceReader>);

/// Optional host projection of command output while the initial invocation waits.
/// Results remain authoritative and cursor-readable; progress is presentation only.
pub trait CommandProgressSink: Send + Sync {
    fn chunk(&self, run: &RunContext, stream: &str, text: &str);
}
pub struct CommandProgressSinkService(pub std::sync::Arc<dyn CommandProgressSink>);

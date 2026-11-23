//! Invocation-time local authority; executors and resource ownership stay native.
use crate::{FileSystemToolsConfig, SystemCommandConfig, filesystem, pty, pty_tools, tasks, tools};
use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, Plugin, PluginContext, PluginError, RunContext, Tool, ToolBinding, ToolDescriptor,
    ToolError, ToolLease, ToolOutput, ToolProvider, ToolProviderPlugin,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalToolGrant {
    /// Trusted terminal owner for product APIs sharing the same service.
    /// None uses the Framework session ID. Never copy this from model input.
    pub terminal_owner: Option<String>,
    pub filesystem: FileSystemToolsConfig,
    pub commands: SystemCommandConfig,
    pub tool_ids: std::collections::BTreeSet<String>,
    /// Optional compatibility names; IDs and executors remain official.
    pub call_names: std::collections::BTreeMap<String, String>,
    /// None exposes all authorized IDs; Some(empty) initially hides all.
    pub exposed_tool_ids: Option<std::collections::BTreeSet<String>>,
}

#[async_trait]
pub trait LocalToolScopeProvider: Send + Sync {
    async fn grant(&self, run: &RunContext) -> Result<LocalToolGrant, AgentError>;

    /// Resolve trusted directory authority at executor entry, after tool admission.
    /// Existing providers get content-addressed versions automatically. Providers
    /// supplying an explicit revision must never reuse it for a different grant.
    async fn snapshot(&self, run: &RunContext) -> Result<LocalToolScopeSnapshot, AgentError> {
        Ok(LocalToolScopeSnapshot::from_grant(self.grant(run).await?))
    }
}

/// One atomic, host-issued grant. A revision identifies the complete authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalToolScopeSnapshot {
    pub revision: String,
    pub grant: LocalToolGrant,
}
impl LocalToolScopeSnapshot {
    pub fn new(revision: impl Into<String>, grant: LocalToolGrant) -> Self {
        Self {
            revision: revision.into(),
            grant,
        }
    }
    /// Stable within this SDK version, without leaking paths in the revision.
    pub fn from_grant(grant: LocalToolGrant) -> Self {
        Self::new(
            format!("{:x}", Sha256::digest(format!("{grant:?}").as_bytes())),
            grant,
        )
    }
}

/// Trusted scope version attached to a native local invocation for audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalToolScopeRevision(pub String);

/// Per-run directory and tool authority with Agent-lifetime command tasks.
/// This is the same executor/catalog used by the individual official plugins.
pub struct ScopedLocalToolsPlugin {
    pty: Option<pty::PtyService>,
    provider: std::sync::Arc<dyn LocalToolScopeProvider>,
    coordination: crate::coordination::CoordinationSelection,
}

impl ScopedLocalToolsPlugin {
    pub fn new(provider: std::sync::Arc<dyn LocalToolScopeProvider>) -> Self {
        Self {
            provider,
            coordination: Default::default(),
            pty: None,
        }
    }
    pub fn with_pty(mut self, service: pty::PtyService) -> Self {
        self.pty = Some(service);
        self
    }
}

struct ScopedLocalTools {
    pty: pty::PtyService,
    provider: Arc<dyn LocalToolScopeProvider>,
    tasks: tasks::CommandTaskManager,
    coordination: crate::coordination::CoordinationSelection,
}

/// Bounded revision history belongs to one run lease, shared by all its tools.
struct ScopeVersion {
    grant: LocalToolGrant,
    bindings: Vec<ToolBinding>,
}
struct ScopeVersions(Mutex<BTreeMap<String, ScopeVersion>>);
impl ScopeVersions {
    fn resolve(
        &self,
        snapshot: &LocalToolScopeSnapshot,
        session: &str,
        tasks: &tasks::CommandTaskManager,
        pty: &pty::PtyService,
        coordination: &crate::coordination::CoordinationSelection,
    ) -> Result<Vec<ToolBinding>, ToolError> {
        if snapshot.revision.is_empty() || snapshot.revision.len() > 512 {
            return Err(ToolError::Failed("invalid local scope revision".into()));
        }
        let mut revisions = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = revisions.get(&snapshot.revision) {
            if previous.grant != snapshot.grant {
                return Err(ToolError::Failed(
                    "local scope revision was reused for different authority".into(),
                ));
            }
        } else {
            if revisions.len() >= 256 {
                return Err(ToolError::Failed(
                    "local scope revision capacity exceeded".into(),
                ));
            }
            let bindings =
                grant_bindings(snapshot.grant.clone(), session, tasks, pty, coordination)
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
            revisions.insert(
                snapshot.revision.clone(),
                ScopeVersion {
                    grant: snapshot.grant.clone(),
                    bindings,
                },
            );
        }
        Ok(revisions[&snapshot.revision].bindings.clone())
    }
}

struct InvocationScopedTool {
    descriptor: ToolDescriptor,
    owner: String,
    provider: Arc<dyn LocalToolScopeProvider>,
    versions: Arc<ScopeVersions>,
    pty: pty::PtyService,
    tasks: tasks::CommandTaskManager,
    coordination: crate::coordination::CoordinationSelection,
}
#[async_trait]
impl Tool for InvocationScopedTool {
    fn direct_batch_input(&self) -> bool {
        matches!(
            self.descriptor.id.as_str(),
            "filesystem.read_text" | "filesystem.write_text"
        )
    }
    async fn prepare(
        &self,
        arguments: &Value,
        run: &RunContext,
    ) -> Result<Option<eliteforge_agent_core::ToolPreparation>, ToolError> {
        let snapshot = self
            .provider
            .snapshot(run)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let binding = self
            .versions
            .resolve(
                &snapshot,
                &run.session_id,
                &self.tasks,
                &self.pty,
                &self.coordination,
            )?
            .into_iter()
            .find(|b| b.descriptor.id == self.descriptor.id)
            .ok_or_else(|| ToolError::Failed("local tool authority revoked".into()))?;
        if binding.descriptor != self.descriptor {
            return Err(ToolError::Failed("local tool contract changed".into()));
        }
        let mut invocation = run.clone();
        invocation
            .extensions
            .insert(LocalToolScopeRevision(snapshot.revision));
        binding.executor.prepare(arguments, &invocation).await
    }
    async fn call(&self, arguments: Value, run: &RunContext) -> Result<ToolOutput, ToolError> {
        run.check_cancelled()
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        let snapshot = tokio::select! {
            biased;
            _ = run.cancelled() => return Err(ToolError::Failed("local scope resolution cancelled".into())),
            _ = tokio::time::sleep_until(run.deadline) => return Err(ToolError::Failed("local scope resolution timed out".into())),
            result = self.provider.snapshot(run) => result.map_err(|error| ToolError::Failed(error.to_string()))?,
        };
        let owner = snapshot
            .grant
            .terminal_owner
            .as_deref()
            .unwrap_or(&run.session_id);
        if owner != self.owner {
            return Err(ToolError::Failed(
                "local terminal ownership cannot change within a run".into(),
            ));
        }
        let binding = self
            .versions
            .resolve(
                &snapshot,
                &run.session_id,
                &self.tasks,
                &self.pty,
                &self.coordination,
            )?
            .into_iter()
            .find(|binding| binding.descriptor.id == self.descriptor.id)
            .ok_or_else(|| ToolError::Failed("local tool authority was revoked".into()))?;
        if binding.descriptor != self.descriptor {
            return Err(ToolError::Failed(
                "local tool contract changed within a run".into(),
            ));
        }
        run.check_cancelled()
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        let mut invocation = run.clone();
        invocation
            .extensions
            .insert(LocalToolScopeRevision(snapshot.revision));
        binding.executor.call(arguments, &invocation).await
    }
}

#[async_trait]
impl ToolProvider for ScopedLocalTools {
    fn catalog_metadata(&self) -> Option<Vec<eliteforge_agent_core::ToolCatalogEntry>> {
        Some(crate::local_tool_catalog_metadata())
    }
    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        run.check_cancelled()?;
        let snapshot = tokio::select! {
            biased;
            _ = run.cancelled() => return Err(AgentError::ToolSetup("local scope lease cancelled".into())),
            _ = tokio::time::sleep_until(run.deadline) => return Err(AgentError::ToolSetup("local scope lease deadline exceeded".into())),
            snapshot = self.provider.snapshot(run) => snapshot?,
        };
        let versions = Arc::new(ScopeVersions(Mutex::new(BTreeMap::new())));
        let owner = snapshot
            .grant
            .terminal_owner
            .clone()
            .unwrap_or_else(|| run.session_id.clone());
        let mut bindings = versions
            .resolve(
                &snapshot,
                &run.session_id,
                &self.tasks,
                &self.pty,
                &self.coordination,
            )
            .map_err(|error| AgentError::ToolSetup(error.to_string()))?;
        for binding in &mut bindings {
            binding.category = Some(eliteforge_agent_core::ToolCategory::localized(
                "system.command",
                "系统命令工具",
                "System command tools",
            ));
            binding.executor = Arc::new(InvocationScopedTool {
                descriptor: binding.descriptor.clone(),
                owner: owner.clone(),
                provider: self.provider.clone(),
                versions: versions.clone(),
                pty: self.pty.clone(),
                tasks: self.tasks.clone(),
                coordination: self.coordination.clone(),
            });
        }
        Ok(ToolLease::new(bindings))
    }
}

fn grant_bindings(
    grant: LocalToolGrant,
    session: &str,
    tasks: &tasks::CommandTaskManager,
    pty: &pty::PtyService,
    coordination: &crate::coordination::CoordinationSelection,
) -> Result<Vec<ToolBinding>, AgentError> {
    let root = filesystem::FileSystemRoot::new(grant.filesystem.clone())
        .with_coordination(coordination.clone());
    let mut bindings: Vec<_> = tools::read_only_bindings(root.clone())
        .into_iter()
        .chain(tools::write_bindings(root))
        .chain(tools::system_command_bindings(
            grant
                .commands
                .clone()
                .with_directory_scope(grant.filesystem.clone()),
            tasks.clone(),
        ))
        .chain(pty_tools::bindings(
            pty.clone(),
            grant
                .terminal_owner
                .clone()
                .unwrap_or_else(|| session.to_owned()),
            grant.commands.with_directory_scope(grant.filesystem),
        ))
        .collect();
    for id in &grant.tool_ids {
        if !bindings.iter().any(|binding| &binding.descriptor.id == id) {
            return Err(AgentError::ToolSetup(format!(
                "unknown official local tool ID: {id}"
            )));
        }
    }
    if grant
        .call_names
        .keys()
        .any(|id| !grant.tool_ids.contains(id))
        || grant
            .exposed_tool_ids
            .as_ref()
            .is_some_and(|ids| !ids.is_subset(&grant.tool_ids))
    {
        return Err(AgentError::ToolSetup(
            "local tool aliases/exposure exceed the host grant".into(),
        ));
    }
    bindings.retain(|binding| grant.tool_ids.contains(&binding.descriptor.id));
    let aliases: Vec<_> = bindings
        .iter()
        .filter_map(|binding| {
            grant
                .call_names
                .get(&binding.descriptor.id)
                .map(|alias| (binding.descriptor.call_name.clone(), alias.clone()))
        })
        .collect();
    for binding in &mut bindings {
        if let Some(name) = grant.call_names.get(&binding.descriptor.id) {
            if name.trim().is_empty() {
                return Err(AgentError::ToolSetup("local tool alias is empty".into()));
            }
            binding.descriptor.call_name = name.clone();
        }
        for (old, new) in &aliases {
            binding.descriptor.description = binding.descriptor.description.replace(old, new);
        }
        if let Some(visible) = &grant.exposed_tool_ids {
            binding.exposed = visible.contains(&binding.descriptor.id);
        }
    }
    Ok(bindings)
}

#[async_trait]
impl Plugin for ScopedLocalToolsPlugin {
    fn id(&self) -> &str {
        "framework.local.scoped.tools"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let pty = self.pty.clone().unwrap_or_default();
        let cleanup_pty = pty.clone();
        context.on_cleanup(move || cleanup_pty.close());
        let shutdown_pty = pty.clone();
        context.on_shutdown(move || async move {
            tokio::task::spawn_blocking(move || shutdown_pty.close())
                .await
                .map_err(|error| error.to_string())
        });
        let tasks = tasks::CommandTaskManager::default();
        let cleanup = tasks.clone();
        context.on_cleanup(move || cleanup.stop_all());
        let shutdown = tasks.clone();
        context.on_shutdown(move || async move { shutdown.shutdown().await });
        ToolProviderPlugin::new(
            self.id(),
            ScopedLocalTools {
                pty,
                provider: self.provider.clone(),
                tasks,
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

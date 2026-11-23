//! Domain selections for official Agent tools. Implementations and resource
//! cleanup remain with the underlying plugins; selecting a pack is explicit.

use std::sync::Arc;

use eliteforge_agent_core::{CapabilityPackBuilder, ConfiguredPluginFactory, capability_factory};
use eliteforge_agent_official_plugin_tools_local::{
    ReadOnlyFileSystemPlugin, SystemCommandPlugin, WriteFileSystemPlugin,
};

pub use eliteforge_agent_official_plugin_tools_mcp::{McpConfig, McpToolsPlugin, McpTransport};

#[cfg(feature = "discovery")]
pub use eliteforge_agent_official_plugin_tools_discovery::ToolDiscoveryPlugin;
#[cfg(feature = "media")]
pub use eliteforge_agent_official_plugin_tools_media::MediaToolsConfig;
#[cfg(feature = "web")]
pub use eliteforge_agent_official_plugin_tools_web::WebToolsPlugin;

pub use eliteforge_agent_official_plugin_tools_local::{
    FileSystemToolsConfig, SystemCommandConfig,
};

pub use eliteforge_agent_official_plugin_tools_local::coordination::{
    FileCoordinationBackend, FileCoordinationCapabilities, FileCoordinationIdentity,
    FileCoordinationLease, FileCoordinationPlugin, FileCoordinationScope, FileEditOutcome,
    FileEditStatus, FileFragment, FileInvalidation, FileObservation, FileResource, FileSubject,
    LocalFileCoordination, default_file_coordination,
};
/// A backend supplied by a capability pack replaces the shared local fallback.
pub fn file_coordination_pack(
    backend: Arc<dyn FileCoordinationBackend>,
) -> CapabilityPackBuilder<()> {
    CapabilityPackBuilder::new("framework.filesystem.coordination", ())
        .plugin(FileCoordinationPlugin(backend))
}

pub const READ_ONLY_FILESYSTEM_PACK: &str = "framework.filesystem.readonly";
pub const WRITE_FILESYSTEM_PACK: &str = "framework.filesystem.write";
pub const SYSTEM_COMMAND_PACK: &str = "framework.system-command";
pub const LOCAL_TOOLS_PACK: &str = "framework.tool-pack.local";
pub const WEB_TOOLS_PACK: &str = "framework.tool-pack.web";
pub const MEDIA_TOOLS_PACK: &str = "framework.tool-pack.media";
pub const MCP_TOOLS_PACK: &str = "framework.tool-pack.mcp";
pub const HUMAN_INTERACTION_PACK: &str = "framework.tool-pack.human";
pub const AGENT_COLLABORATION_PACK: &str = "framework.tool-pack.agent-collaboration";
pub const TOOL_DISCOVERY_PACK: &str = "framework.tool-pack.discovery";

/// Choose the local tool groups granted to one Agent. An empty selection grants
/// no local tools. The existing individual packs remain available for callers
/// that want to compose only one group.
#[derive(Default)]
pub struct LocalToolsConfig {
    read_only_filesystem: Option<FileSystemToolsConfig>,
    write_filesystem: Option<FileSystemToolsConfig>,
    system_commands: Option<SystemCommandConfig>,
    pty: Option<PtyService>,
}

impl LocalToolsConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read_only_filesystem(mut self, config: FileSystemToolsConfig) -> Self {
        self.read_only_filesystem = Some(config);
        self
    }

    pub fn write_filesystem(mut self, config: FileSystemToolsConfig) -> Self {
        self.write_filesystem = Some(config);
        self
    }

    pub fn system_commands(mut self, config: SystemCommandConfig) -> Self {
        self.system_commands = Some(config);
        self
    }

    pub fn pty(mut self, service: PtyService) -> Self {
        self.pty = Some(service);
        self
    }
}

/// Compose the selected local groups without creating a second copy of any
/// provider. PTY and scoped local tools retain their established lifecycles.
pub fn local_tools_pack(config: LocalToolsConfig) -> CapabilityPackBuilder<()> {
    let mut pack = CapabilityPackBuilder::new(LOCAL_TOOLS_PACK, ());
    if let Some(read) = config.read_only_filesystem {
        pack = pack.include(read_only_filesystem_pack(read));
    }
    if let Some(write) = config.write_filesystem {
        pack = pack.include(write_filesystem_pack(write));
    }
    if let Some(commands) = config.system_commands {
        pack = pack.include(system_command_pack(commands));
    }
    if let Some(pty) = config.pty {
        pack = pack.include(pty_pack(pty));
    }
    pack
}

/// Host-configured search and fetch backends are retained across idle
/// reconfiguration. The same configured provider must not be installed again.
#[cfg(feature = "web")]
pub fn web_tools_pack(plugin: WebToolsPlugin) -> CapabilityPackBuilder<()> {
    CapabilityPackBuilder::new(WEB_TOOLS_PACK, ()).plugin(plugin)
}

/// Local image and PDF readers can be recreated from their configuration.
#[cfg(feature = "media")]
pub fn media_tools_pack(config: MediaToolsConfig) -> CapabilityPackBuilder<()> {
    let plugin = eliteforge_agent_official_plugin_tools_media::MediaToolsPlugin::new(config);
    CapabilityPackBuilder::new(MEDIA_TOOLS_PACK, ())
        .plugin_factory(ConfiguredPluginFactory::from_config(plugin))
}

/// Group every configured MCP server under one pack. The empty selection
/// preserves the standard empty MCP source without opening a connection.
/// Configured servers must have distinct plugin IDs.
pub fn mcp_tools_pack(
    servers: impl IntoIterator<Item = McpToolsPlugin>,
) -> CapabilityPackBuilder<()> {
    let mut pack = CapabilityPackBuilder::new(MCP_TOOLS_PACK, ());
    let mut servers: Vec<_> = servers.into_iter().collect();
    if servers.is_empty() {
        servers.push(McpToolsPlugin::default());
    }
    for server in servers {
        pack = pack.plugin_factory(ConfiguredPluginFactory::from_config(server));
    }
    pack
}

/// Approval and user-input tools, created once per Agent installation.
pub fn human_interaction_pack() -> CapabilityPackBuilder<()> {
    CapabilityPackBuilder::new(HUMAN_INTERACTION_PACK, ()).plugin_factory(
        ConfiguredPluginFactory::new("tools.human", vec![], || {
            Ok(Arc::new(
                eliteforge_agent_official_plugin_tools_human::HumanInteractionPlugin,
            ))
        }),
    )
}

/// Child-agent and thread tools, created once per Agent installation.
pub fn agent_collaboration_pack() -> CapabilityPackBuilder<()> {
    CapabilityPackBuilder::new(AGENT_COLLABORATION_PACK, ()).plugin_factory(
        ConfiguredPluginFactory::new("tools.agents", vec![], || {
            Ok(Arc::new(
                eliteforge_agent_official_plugin_tools_agents::AgentToolsPlugin,
            ))
        }),
    )
}

/// A configured discovery provider stays installed; its host-supplied
/// providers are leased once per run and must not also be installed separately.
#[cfg(feature = "discovery")]
pub fn tool_discovery_pack(plugin: ToolDiscoveryPlugin) -> CapabilityPackBuilder<()> {
    CapabilityPackBuilder::new(TOOL_DISCOVERY_PACK, ()).plugin(plugin)
}

/// Select the read-only filesystem tools for one Agent.
pub fn read_only_filesystem_pack(
    config: FileSystemToolsConfig,
) -> CapabilityPackBuilder<FileSystemToolsConfig> {
    CapabilityPackBuilder::new(READ_ONLY_FILESYSTEM_PACK, config)
}

/// Select the mutating filesystem tools for one Agent.
pub fn write_filesystem_pack(
    config: FileSystemToolsConfig,
) -> CapabilityPackBuilder<FileSystemToolsConfig> {
    CapabilityPackBuilder::new(WRITE_FILESYSTEM_PACK, config)
}

/// Select literal command execution and its read/stop task tools for one Agent.
pub fn system_command_pack(
    config: SystemCommandConfig,
) -> CapabilityPackBuilder<SystemCommandConfig> {
    CapabilityPackBuilder::new(SYSTEM_COMMAND_PACK, config)
}

#[capability_factory(
    pack = "framework.filesystem.readonly",
    id = "framework.filesystem.readonly.tools",
    config = FileSystemToolsConfig
)]
fn read_only_tools(config: &FileSystemToolsConfig) -> ReadOnlyFileSystemPlugin {
    ReadOnlyFileSystemPlugin::new(config.clone())
}

#[capability_factory(
    pack = "framework.filesystem.write",
    id = "framework.filesystem.write.tools",
    config = FileSystemToolsConfig
)]
fn write_tools(config: &FileSystemToolsConfig) -> WriteFileSystemPlugin {
    WriteFileSystemPlugin::new(config.clone())
}

#[capability_factory(
    pack = "framework.system-command",
    id = "framework.system-command.tools",
    config = SystemCommandConfig
)]
fn command_tools(config: &SystemCommandConfig) -> SystemCommandPlugin {
    SystemCommandPlugin::new(config.clone())
}

pub use eliteforge_agent_official_plugin_tools_local::{
    PtyCommand,
    pty::{PtyEvent, PtyService, PtySnapshot, PtyStart},
};
pub const PTY_PACK: &str = "framework.pty";
/// Share one framework-owned terminal service between Agent and desktop adapters.
/// Hosts may construct the service before a lazy Agent starts; explicit close and
/// last-owner drop also terminate sessions when the Agent was never initialized.
pub fn pty_pack(service: PtyService) -> CapabilityPackBuilder<PtyService> {
    CapabilityPackBuilder::new(PTY_PACK, service)
}
#[capability_factory(pack = "framework.pty", id = "framework.pty.service", config = PtyService)]
fn pty_service(
    service: &PtyService,
) -> eliteforge_agent_official_plugin_tools_local::pty::PtyPlugin {
    eliteforge_agent_official_plugin_tools_local::pty::PtyPlugin(service.clone())
}

/// Scoped local tools and PTY are assembled together with one shared lifecycle.
pub fn scoped_local_tools_pack(
    provider: std::sync::Arc<
        dyn eliteforge_agent_official_plugin_tools_local::LocalToolScopeProvider,
    >,
    service: PtyService,
) -> CapabilityPackBuilder<()> {
    CapabilityPackBuilder::new("framework.local.scoped", ())
        .include(pty_pack(service.clone()))
        .plugin(
            eliteforge_agent_official_plugin_tools_local::ScopedLocalToolsPlugin::new(provider)
                .with_pty(service),
        )
}

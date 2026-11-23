use eliteforge_agent_test_support::TestBuilder;
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eliteforge_agent_capability_tools::{
    FileSystemToolsConfig, LocalToolsConfig, McpConfig, McpToolsPlugin, McpTransport,
    SystemCommandConfig, agent_collaboration_pack, human_interaction_pack, local_tools_pack,
    mcp_tools_pack, read_only_filesystem_pack, system_command_pack, write_filesystem_pack,
};
use eliteforge_agent_core::*;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestConversationStore;
use serde_json::{Value, json};

fn plugin_ids(pack: &impl CapabilityPack) -> Vec<String> {
    pack.plugins()
        .iter()
        .map(|plugin| plugin.id().to_owned())
        .collect()
}

fn builder(agent_loop: impl AgentLoop + 'static) -> TestBuilder {
    TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(agent_loop))
        .plugin(ConversationStorePlugin::new(
            TestConversationStore::default(),
        ))
}

fn completed() -> RunResult {
    // The external Agent runtime adds an optional session reference to the
    // shared result. Keeping this fixture wire-shaped works across that API
    // addition without changing what the tool tests observe.
    serde_json::from_value(json!({
        "output": "",
        "content": Content::default(),
        "structured": null,
        "status": RunStatus::Completed,
        "interactions": [],
        "usage": Usage::default(),
        "steps": 0,
        "tool_calls": 0,
        "external_session": null,
    }))
    .expect("completed result should deserialize")
}

struct ListToolsLoop(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl AgentLoop for ListToolsLoop {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut names = Vec::new();
        for provider in &services.tools {
            names.extend(
                provider
                    .lease(context)
                    .await?
                    .tools
                    .into_iter()
                    .map(|binding| binding.descriptor.call_name),
            );
        }
        *self.0.lock().unwrap() = names;
        Ok(completed())
    }
}

async fn listed_tools(pack: impl CapabilityPack + 'static) -> Vec<String> {
    let names = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(ListToolsLoop(names.clone()))
        .capability_pack(pack)
        .build()
        .await
        .expect("selected domain pack should build");
    agent
        .run(RunRequest::new("tool-domain", "list"))
        .await
        .unwrap();
    assert!(agent.framework().close().await.is_success());
    names.lock().unwrap().clone()
}

struct InvokeToolLoop {
    call_name: &'static str,
    arguments: Value,
    output: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl AgentLoop for InvokeToolLoop {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        for provider in &services.tools {
            let lease = provider.lease(context).await?;
            let Some(binding) = lease
                .tools
                .iter()
                .find(|binding| binding.descriptor.call_name == self.call_name)
            else {
                continue;
            };
            let output = binding
                .executor
                .call(self.arguments.clone(), context)
                .await
                .map_err(|error| AgentError::Run(error.to_string()))?;
            *self
                .output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = output.structured;
            return Ok(completed());
        }
        Err(AgentError::Run(format!(
            "tool `{}` was not provided by the selected packs",
            self.call_name
        )))
    }
}

async fn invoke(
    pack_builder: impl CapabilityPack + 'static,
    call_name: &'static str,
    arguments: Value,
) -> Value {
    let output = Arc::new(Mutex::new(None));
    let agent = builder(InvokeToolLoop {
        call_name,
        arguments,
        output: output.clone(),
    })
    .capability_pack(pack_builder)
    .build()
    .await
    .expect("selected capability pack should build");
    agent
        .run(RunRequest::new("filesystem-tools", "invoke"))
        .await
        .expect("tool loop should complete");
    assert!(agent.framework().close().await.is_success());
    output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .expect("tool output should be structured JSON")
}

async fn invoke_error(
    pack_builder: impl CapabilityPack + 'static,
    call_name: &'static str,
    arguments: Value,
) -> String {
    let output = Arc::new(Mutex::new(None));
    let agent = builder(InvokeToolLoop {
        call_name,
        arguments,
        output,
    })
    .capability_pack(pack_builder)
    .build()
    .await
    .expect("selected capability pack should build");
    let error = agent
        .run(RunRequest::new("filesystem-tools", "reject path escape"))
        .await
        .expect_err("filesystem tools must reject paths outside the configured root");
    assert!(agent.framework().close().await.is_success());
    error.to_string()
}

struct ToolHarness {
    agent: Agent,
    invocation: Arc<Mutex<(&'static str, Value)>>,
    output: Arc<Mutex<Option<Value>>>,
}

struct CommandTaskLoop {
    invocation: Arc<Mutex<(&'static str, Value)>>,
    output: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl AgentLoop for CommandTaskLoop {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let (call_name, arguments) = self.invocation.lock().unwrap().clone();
        InvokeToolLoop {
            call_name,
            arguments,
            output: self.output.clone(),
        }
        .run(request, context, services)
        .await
    }
}

impl ToolHarness {
    #[cfg(unix)]
    async fn new(config: SystemCommandConfig) -> Self {
        let invocation = Arc::new(Mutex::new(("command_exec", json!({}))));
        let output = Arc::new(Mutex::new(None));
        let agent = builder(CommandTaskLoop {
            invocation: invocation.clone(),
            output: output.clone(),
        })
        .capability_pack(system_command_pack(config))
        .build()
        .await
        .unwrap();
        Self {
            agent,
            invocation,
            output,
        }
    }

    async fn filesystem(config: FileSystemToolsConfig) -> Self {
        let invocation = Arc::new(Mutex::new(("fs_read_text", json!({}))));
        let output = Arc::new(Mutex::new(None));
        let agent = builder(CommandTaskLoop {
            invocation: invocation.clone(),
            output: output.clone(),
        })
        .capability_pack(read_only_filesystem_pack(config.clone()))
        .capability_pack(write_filesystem_pack(config))
        .build()
        .await
        .unwrap();
        Self {
            agent,
            invocation,
            output,
        }
    }

    async fn call(
        &self,
        session: &str,
        call_name: &'static str,
        arguments: Value,
    ) -> Result<Value, AgentError> {
        *self.invocation.lock().unwrap() = (call_name, arguments);
        *self.output.lock().unwrap() = None;
        self.agent
            .run(RunRequest::new(session, "command task"))
            .await?;
        Ok(self
            .output
            .lock()
            .unwrap()
            .clone()
            .expect("structured tool result"))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn command_task_survives_runs_and_supports_cursor_reads_session_checks_and_stop() {
    let harness =
        ToolHarness::new(SystemCommandConfig::new().with_async_after(Duration::from_millis(20)))
            .await;
    let started = harness
        .call(
            "owner",
            "command_exec",
            json!({
                "command": "echo first; echo second; exec sleep 30"
            }),
        )
        .await
        .unwrap();
    assert_eq!(started["status"], "running");
    let task_id = &started["taskId"];
    let denied = harness
        .call(
            "foreign",
            "command_exec_read",
            json!({
                "taskId": task_id
            }),
        )
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("was not found"));
    let next = harness
        .call(
            "owner",
            "command_exec_read",
            json!({
                "taskId": task_id,
                "stdoutCursor": started["stdout"]["nextCursor"],
                "stderrCursor": started["stderr"]["nextCursor"],
                "waitMs": 1000
            }),
        )
        .await
        .unwrap();
    let all_output = format!(
        "{}{}",
        started["stdout"]["content"].as_str().unwrap(),
        next["stdout"]["content"].as_str().unwrap(),
    );
    assert!(all_output.contains("first\nsecond\n"));
    let stopped = harness
        .call(
            "owner",
            "command_exec_stop",
            json!({
                "taskId": task_id,
                "stdoutCursor": next["stdout"]["nextCursor"],
                "stderrCursor": next["stderr"]["nextCursor"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(stopped["status"], "cancelled");
    assert_eq!(stopped["stopRequested"], true);
    assert_eq!(stopped["stdout"]["content"], "");
    assert!(harness.agent.framework().close().await.is_success());
}

#[cfg(unix)]
#[tokio::test]
async fn default_command_handoff_returns_after_ten_seconds() {
    let harness = ToolHarness::new(SystemCommandConfig::new()).await;
    let start = Instant::now();
    let running = tokio::time::timeout(
        Duration::from_secs(15),
        harness.call(
            "owner",
            "command_exec",
            json!({"command": "sleep 30"}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(start.elapsed() >= Duration::from_secs(10));
    assert_eq!(running["status"], "running");
    assert!(running["taskId"].as_str().unwrap().starts_with("cmd-"));
    let stopped = harness
        .call(
            "owner",
            "command_exec_stop",
            json!({
                "taskId": running["taskId"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(stopped["status"], "cancelled");
    assert!(harness.agent.framework().close().await.is_success());
}

#[cfg(unix)]
#[tokio::test]
async fn closing_framework_terminates_its_background_command() {
    let harness =
        ToolHarness::new(SystemCommandConfig::new().with_async_after(Duration::from_millis(200)))
            .await;
    let running = harness
        .call(
            "owner",
            "command_exec",
            json!({
                "command": "echo $$; exec sleep 30"
            }),
        )
        .await
        .unwrap();
    let pid: u32 = running["stdout"]["content"].as_str().unwrap().trim().parse().unwrap();
    let is_running = || {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(is_running());
    assert!(harness.agent.framework().close().await.is_success());
    assert!(
        !is_running(),
        "Agent shutdown must reap the command process"
    );
}

#[tokio::test]
async fn readonly_and_write_filesystem_packs_operate_inside_the_configured_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("notes.txt"),
        "first line\nneedle\nlast line\n",
    )
    .unwrap();
    let config = FileSystemToolsConfig::new(directory.path());

    let read = invoke(
        read_only_filesystem_pack(config.clone()),
        "fs_read_text",
        json!({"path":"notes.txt", "startLine":2}),
    )
    .await;
    assert_eq!(read["content"], json!("needle\nlast line"));

    let grep = invoke(
        read_only_filesystem_pack(config.clone()),
        "search_grep",
        json!({"pattern":"needle", "glob":["*.txt"]}),
    )
    .await;
    assert_eq!(grep["matches"][0]["path"], json!("notes.txt"));
    assert_eq!(grep["matches"][0]["line"], json!(2));

    let write = invoke(
        write_filesystem_pack(config.clone()),
        "fs_write_text",
        json!({"path":"nested/generated.txt", "content":"draft", "mode":"create"}),
    )
    .await;
    assert_eq!(write["bytesWritten"], json!(5));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("nested/generated.txt")).unwrap(),
        "draft"
    );

    let patch = invoke(
        write_filesystem_pack(config),
        "fs_apply_patch",
        json!({"path":"nested/generated.txt", "action":"replace", "target":"draft", "content":"done"}),
    )
    .await;
    assert_eq!(patch["replacements"], json!(1));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("nested/generated.txt")).unwrap(),
        "done"
    );
}

#[tokio::test]
async fn local_domain_pack_combines_selected_tools_without_duplicate_providers() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "from local pack").unwrap();
    let config = FileSystemToolsConfig::new(directory.path());
    let selection = || {
        local_tools_pack(
            LocalToolsConfig::new()
                .read_only_filesystem(config.clone())
                .write_filesystem(config.clone())
                .system_commands(SystemCommandConfig::new()),
        )
    };
    let tools = listed_tools(selection()).await;
    assert_eq!(
        tools.iter().filter(|name| *name == "fs_read_text").count(),
        1
    );
    assert_eq!(
        tools.iter().filter(|name| *name == "fs_write_text").count(),
        1
    );
    assert_eq!(
        tools.iter().filter(|name| *name == "command_exec").count(),
        1
    );
    let read = invoke(selection(), "fs_read_text", json!({"path":"note.txt"})).await;
    assert_eq!(read["content"], "from local pack");
}

#[tokio::test]
async fn human_and_collaboration_domain_packs_keep_official_tool_descriptors() {
    assert_eq!(plugin_ids(&human_interaction_pack()), ["tools.human"]);
    assert_eq!(plugin_ids(&agent_collaboration_pack()), ["tools.agents"]);

    let names = listed_tools(
        CapabilityPackBuilder::new("test.tool-domains", ())
            .include(human_interaction_pack())
            .include(agent_collaboration_pack()),
    )
    .await;
    assert!(names.contains(&"ask_user".to_owned()));
    assert!(names.contains(&"agent_spawn".to_owned()));
    assert!(names.contains(&"thread_create".to_owned()));
}

#[tokio::test]
async fn mcp_domain_pack_keeps_one_empty_default_and_multiple_named_servers() {
    let empty = mcp_tools_pack(Vec::<McpToolsPlugin>::new());
    assert_eq!(plugin_ids(&empty), ["tools.mcp"]);
    assert!(listed_tools(empty).await.is_empty());

    let server = |id: &str| {
        McpToolsPlugin::new(
            id,
            McpConfig::new(
                id,
                McpTransport::StreamableHttp {
                    url: "http://127.0.0.1:1/mcp".into(),
                    headers: Default::default(),
                },
            ),
        )
    };
    let named = mcp_tools_pack([server("mcp.first"), server("mcp.second")]);
    assert_eq!(plugin_ids(&named), ["mcp.first", "mcp.second"]);
}

#[tokio::test]
async fn mcp_domain_pack_rejects_duplicate_server_plugin_ids_before_connection() {
    let result = builder(ListToolsLoop(Arc::new(Mutex::new(Vec::new()))))
        .capability_pack(mcp_tools_pack([
            McpToolsPlugin::default(),
            McpToolsPlugin::default(),
        ]))
        .build()
        .await;
    assert!(matches!(
        result,
        Err(AgentError::Plugin(PluginError::DuplicatePluginId { id })) if id == "tools.mcp"
    ));
}

#[cfg(feature = "web")]
#[tokio::test]
async fn web_domain_pack_retains_host_backend_and_exposes_only_selected_tools() {
    use eliteforge_agent_capability_tools::{WebToolsPlugin, web_tools_pack};
    use eliteforge_agent_official_plugin_tools_web::WebFetchConfig;

    let pack =
        web_tools_pack(WebToolsPlugin::new().fetch(WebFetchConfig::restricted(["127.0.0.1"])));
    assert_eq!(plugin_ids(&pack), ["framework.web.tools"]);
    assert_eq!(listed_tools(pack).await, ["web_read"]);
}

#[cfg(feature = "media")]
#[tokio::test]
async fn media_domain_pack_recreates_reader_and_exposes_image_and_pdf_tools() {
    use eliteforge_agent_capability_tools::{MediaToolsConfig, media_tools_pack};

    let root = tempfile::tempdir().unwrap();
    let pack = media_tools_pack(MediaToolsConfig::new(root.path()));
    assert_eq!(plugin_ids(&pack), ["framework.media.tools"]);
    assert_eq!(
        listed_tools(pack).await,
        ["media_read_image", "media_read_pdf"]
    );
}

#[cfg(feature = "discovery")]
#[tokio::test]
async fn discovery_domain_pack_retains_host_providers_and_exposes_the_gateway_trio() {
    use eliteforge_agent_capability_tools::{ToolDiscoveryPlugin, tool_discovery_pack};

    let pack = tool_discovery_pack(ToolDiscoveryPlugin::new());
    assert_eq!(plugin_ids(&pack), ["framework.tools.discovery"]);
    // With no explicitly wrapped providers the gateway trio is the whole
    // model-visible catalog: search, schema and the execution envelope.
    assert_eq!(
        listed_tools(pack).await,
        ["tool_search", "tool_schema", "tool_execute"]
    );
}

#[tokio::test]
async fn filesystem_packs_reject_parent_directory_escape() {
    let directory = tempfile::tempdir().unwrap();
    let error = invoke_error(
        read_only_filesystem_pack(FileSystemToolsConfig::new(directory.path())),
        "fs_read_text",
        json!({"path":"../outside.txt"}),
    )
    .await;
    assert!(error.contains("path must be relative to the configured filesystem root"));
}

#[cfg(unix)]
#[tokio::test]
async fn filesystem_packs_reject_symbolic_links() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), directory.path().join("outside-link")).unwrap();
    let error = invoke_error(
        read_only_filesystem_pack(FileSystemToolsConfig::new(directory.path())),
        "fs_read_text",
        json!({"path":"outside-link"}),
    )
    .await;
    assert!(error.contains("symbolic links are not allowed"));
}

#[tokio::test]
async fn system_command_pack_uses_platform_native_command_runners() {
    #[cfg(windows)]
    let command = "cmd /C echo cross-platform";
    #[cfg(not(windows))]
    let command = "printf cross-platform";

    let output = invoke(
        system_command_pack(SystemCommandConfig::new()),
        "command_exec",
        json!({"command":command}),
    )
    .await;
    assert_eq!(output["success"], json!(true));
    assert!(
        output["stdout"]["content"]
            .as_str()
            .expect("command output should be text")
            .contains("cross-platform")
    );
}

#[tokio::test]
async fn system_command_pack_rejects_legacy_args() {
    let error = invoke_error(
        system_command_pack(SystemCommandConfig::new()),
        "command_exec",
        json!({"command":"echo ok","args":["legacy"]}),
    )
    .await;
    assert!(error.contains("no longer accepts args"));
}

#[tokio::test]
async fn system_command_pack_accepts_an_arbitrary_working_directory() {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let command = "cmd /C cd";
    #[cfg(not(windows))]
    let command = "pwd";

    let output = invoke(
        system_command_pack(SystemCommandConfig::new()),
        "command_exec",
        json!({
            "command":command,
            "cwd":directory.path().to_string_lossy(),
        }),
    )
    .await;

    let actual = std::fs::canonicalize(
        output["stdout"]["content"]
            .as_str()
            .expect("command output should be text")
            .trim(),
    )
    .expect("command must run in the requested directory");
    assert_eq!(actual, std::fs::canonicalize(directory.path()).unwrap());
}

#[cfg(unix)]
async fn command_until_terminal(harness: &ToolHarness, task: &Value) -> Value {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let result = harness
                .call(
                    "owner",
                    "command_exec_read",
                    json!({"taskId":task,"waitMs":100}),
                )
                .await
                .unwrap();
            if result["status"] != "running" {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("command should terminate")
}

#[cfg(unix)]
async fn assert_process_gone(pid: u32) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let running = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .unwrap()
                .status
                .success();
            if !running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("managed process should be gone");
}

#[cfg(unix)]
#[tokio::test]
async fn interactive_command_accepts_multiple_writes_and_eof_with_scoped_command_inventory() {
    let harness =
        ToolHarness::new(SystemCommandConfig::new().with_async_after(Duration::from_millis(30)))
            .await;
    let started = harness.call("owner", "command_exec", json!({
        "command":r#"read first; printf "first=%s\n" "$first"; cat"#, "interactive":true
    })).await.unwrap();
    assert_eq!(started["stdinOpen"], true);
    assert_eq!(started["tty"], false);
    let id = &started["taskId"];
    let list = harness
        .call("owner", "command_exec_list", json!({}))
        .await
        .unwrap();
    assert_eq!(list["commands"].as_array().unwrap().len(), 1);
    assert_eq!(list["commands"][0]["taskId"], *id);
    let foreign = harness
        .call(
            "foreign",
            "command_exec_list",
            json!({"includeCompleted":true}),
        )
        .await
        .unwrap();
    assert!(foreign["commands"].as_array().unwrap().is_empty());
    assert!(
        harness
            .call(
                "foreign",
                "command_exec_write",
                json!({"taskId":id,"data":"wrong\n"})
            )
            .await
            .is_err()
    );
    harness
        .call(
            "owner",
            "command_exec_write",
            json!({"taskId":id,"data":"hello\n"}),
        )
        .await
        .unwrap();
    harness
        .call(
            "owner",
            "command_exec_write",
            json!({"taskId":id,"data":"world\n","closeStdin":true}),
        )
        .await
        .unwrap();
    let finished = command_until_terminal(&harness, id).await;
    assert_eq!(finished["stdout"]["content"], "first=hello\nworld\n");
    assert_eq!(finished["success"], true);
    assert_eq!(finished["stdinOpen"], false);
    assert!(
        harness
            .call(
                "owner",
                "command_exec_write",
                json!({"taskId":id,"data":"late"})
            )
            .await
            .is_err()
    );
    let running = harness
        .call("owner", "command_exec_list", json!({}))
        .await
        .unwrap();
    assert!(running["commands"].as_array().unwrap().is_empty());
    let history = harness
        .call(
            "owner",
            "command_exec_list",
            json!({"includeCompleted":true}),
        )
        .await
        .unwrap();
    assert_eq!(history["commands"][0]["status"], "completed");
    assert!(harness.agent.framework().close().await.is_success());
}

#[cfg(unix)]
#[tokio::test]
async fn pty_is_a_real_terminal_and_combines_output_and_accepts_input() {
    let harness =
        ToolHarness::new(SystemCommandConfig::new().with_async_after(Duration::from_millis(50)))
            .await;
    let started = harness.call("owner", "command_exec", json!({
        "command":r#"test -t 0 && test -t 1 && test -t 2 || exit 9; stty -echo; stty size; read answer; printf "answer=%s\n" "$answer"; printf "error-stream\n" >&2"#,
        "tty":true,"rows":40,"cols":100
    })).await.unwrap();
    assert_eq!(started["tty"], true);
    let id = &started["taskId"];
    harness
        .call(
            "owner",
            "command_exec_write",
            json!({"taskId":id,"data":"terminal-value\n"}),
        )
        .await
        .unwrap();
    let finished = command_until_terminal(&harness, id).await;
    assert_eq!(finished["success"], true, "{finished}");
    let output = finished["stdout"]["content"].as_str().unwrap().replace("\r\n", "\n");
    assert!(output.contains("40 100\n"), "{output}");
    assert!(output.contains("answer=terminal-value\n"), "{output}");
    assert!(output.contains("error-stream\n"), "{output}");
    assert_eq!(finished["stderr"]["content"], "");
    assert!(harness.agent.framework().close().await.is_success());
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_pipe_and_pty_commands_cleans_their_process_groups() {
    for tty in [false, true] {
        let harness = ToolHarness::new(
            SystemCommandConfig::new().with_async_after(Duration::from_millis(80)),
        )
        .await;
        let started = harness
            .call(
                "owner",
                "command_exec",
                json!({
                    "command":"sleep 30 & echo $!; wait", "tty":tty
                }),
            )
            .await
            .unwrap();
        let output = started["stdout"]["content"].as_str().unwrap().trim();
        let descendant: u32 = output.parse().expect("shell should report child pid");
        let stopped = harness
            .call(
                "owner",
                "command_exec_stop",
                json!({"taskId":started["taskId"]}),
            )
            .await
            .unwrap();
        assert_eq!(stopped["status"], "cancelled");
        assert_process_gone(started["pid"].as_u64().unwrap() as u32).await;
        assert_process_gone(descendant).await;
        assert!(harness.agent.framework().close().await.is_success());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_and_framework_close_clean_managed_descendants() {
    for timed_out in [false, true] {
        let config = SystemCommandConfig::new()
            .with_async_after(Duration::from_millis(50))
            .with_timeout(if timed_out {
                Duration::from_millis(300)
            } else {
                Duration::from_secs(30)
            });
        let harness = ToolHarness::new(config).await;
        let started = harness
            .call(
                "owner",
                "command_exec",
                json!({
                    "command":"sleep 30 & echo $!; wait", "tty":true
                }),
            )
            .await
            .unwrap();
        let descendant: u32 = started["stdout"]["content"].as_str().unwrap().trim().parse().unwrap();
        if timed_out {
            let finished = command_until_terminal(&harness, &started["taskId"]).await;
            assert_eq!(finished["status"], "timed_out");
        }
        assert!(harness.agent.framework().close().await.is_success());
        assert_process_gone(descendant).await;
        assert_process_gone(started["pid"].as_u64().unwrap() as u32).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn completed_shell_does_not_leave_background_children_and_plain_stdin_still_closes() {
    let harness = ToolHarness::new(SystemCommandConfig::new()).await;
    let finished = harness
        .call(
            "owner",
            "command_exec",
            json!({
                "command":"sleep 30 & echo $!"
            }),
        )
        .await
        .unwrap();
    assert_eq!(finished["status"], "completed");
    assert_process_gone(finished["stdout"]["content"].as_str().unwrap().trim().parse().unwrap()).await;
    let finished = harness
        .call(
            "owner",
            "command_exec",
            json!({"command":"cat","stdin":"one-shot\n"}),
        )
        .await
        .unwrap();
    assert_eq!(finished["stdout"]["content"], "one-shot\n");
    assert_eq!(finished["success"], true);
    assert!(harness.agent.framework().close().await.is_success());
}

#[tokio::test]
async fn unified_patch_accepts_relative_versions_with_authorized_roots() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("note.txt");
    std::fs::write(&path, "old\n").unwrap();
    let config = FileSystemToolsConfig::new(root.path())
        .with_authorized_roots(vec![root.path().to_path_buf()]);
    let harness = ToolHarness::filesystem(config).await;
    let read = harness
        .call("owner", "fs_read_text", json!({"path":"note.txt"}))
        .await
        .unwrap();
    let patch = "--- note.txt\n+++ note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let duplicate = json!({"patch":patch,"expectedVersions":{"note.txt":read["version"],(path.to_string_lossy().to_string()):read["version"]}});
    assert!(
        harness
            .call("owner", "fs_apply_unified_patch", duplicate)
            .await
            .unwrap_err()
            .to_string()
            .contains("duplicate paths")
    );
    let result = harness
        .call(
            "owner",
            "fs_apply_unified_patch",
            json!({"patch":patch,"expectedVersions":{"note.txt":read["version"]}}),
        )
        .await
        .unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "new\n");
    assert!(harness.agent.framework().close().await.is_success());
}

#[tokio::test]
async fn unified_patch_previews_versions_creates_deletes_and_rejects_stale_writes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "old\n").unwrap();
    std::fs::write(root.path().join("b.txt"), "remove\n").unwrap();
    let harness = ToolHarness::filesystem(FileSystemToolsConfig::new(root.path())).await;
    let a = harness
        .call("owner", "fs_read_text", json!({"path":"a.txt"}))
        .await
        .unwrap();
    let b = harness
        .call("owner", "fs_read_text", json!({"path":"b.txt"}))
        .await
        .unwrap();
    let patch = "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-old\n+new\n--- b.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-remove\n--- /dev/null\n+++ nested/c.txt\n@@ -0,0 +1 @@\n+created\n";
    let mut args = json!({"patch":patch,"expectedVersions":{"a.txt":a["version"],"b.txt":b["version"],"nested/c.txt":"absent"},"dryRun":true});
    let preview = harness
        .call("owner", "fs_apply_unified_patch", args.clone())
        .await
        .unwrap();
    assert_eq!(preview["applied"], false);
    assert!(
        preview["changes"][0]["diff"]
            .as_str()
            .unwrap()
            .contains("+new")
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "old\n"
    );
    assert!(!root.path().join("nested/c.txt").exists());
    args["dryRun"] = json!(false);
    let result = harness
        .call("owner", "fs_apply_unified_patch", args.clone())
        .await
        .unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "new\n"
    );
    assert!(!root.path().join("b.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("nested/c.txt")).unwrap(),
        "created\n"
    );
    let error = harness
        .call(
            "owner",
            "fs_write_text",
            json!({"path":"a.txt","content":"bad","expectedVersion":a["version"]}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("version conflict"));
    let error = harness.call("owner", "fs_apply_patch", json!({"path":"a.txt","action":"replace","target":"new","content":"bad","expectedVersion":a["version"]})).await.unwrap_err();
    assert!(error.to_string().contains("version conflict"));
    assert!(
        harness
            .call("owner", "fs_apply_unified_patch", args)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "new\n"
    );
    assert!(harness.agent.framework().close().await.is_success());
}

#[tokio::test]
async fn patch_validates_all_files_before_changing_anything_and_honors_limits() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), "old\n").unwrap();
    let harness =
        ToolHarness::filesystem(FileSystemToolsConfig::new(root.path()).with_max_edit_bytes(64))
            .await;
    let read = harness
        .call("owner", "fs_read_text", json!({"path":"a"}))
        .await
        .unwrap();
    let error = harness.call("owner", "fs_apply_unified_patch", json!({
        "patch":"--- a\n+++ a\n@@ -1 +1 @@\n-old\n+new\n--- ../escape\n+++ ../escape\n@@ -1 +1 @@\n-x\n+y\n",
        "expectedVersions":{"a":read["version"],"../escape":"absent"}
    })).await.unwrap_err();
    assert!(error.to_string().contains("relative"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("a")).unwrap(),
        "old\n"
    );
    assert!(
        harness
            .call(
                "owner",
                "fs_write_text",
                json!({"path":"a","content":"x".repeat(65)})
            )
            .await
            .is_err()
    );
    std::fs::write(root.path().join("huge"), "x".repeat(65)).unwrap();
    assert!(
        harness
            .call("owner", "fs_read_text", json!({"path":"huge"}))
            .await
            .is_err()
    );
    let git = harness.call("owner", "fs_apply_unified_patch", json!({"format":"git","patch":"diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+git\n","expectedVersions":{"a":read["version"]}})).await.unwrap();
    assert_eq!(git["applied"], true);
    assert!(harness.agent.framework().close().await.is_success());
}

#[tokio::test]
async fn search_supports_globs_ignore_excludes_modes_and_stable_scoped_pages() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    for (name, text) in [
        ("a.rs", "a.b\naXb\n"),
        ("src/b.rs", "a.b\n"),
        ("src/c.txt", "a.b\n"),
        ("ignored.rs", "a.b\n"),
        (".hidden.rs", "a.b\n"),
        (".gitignore", "ignored.rs\n"),
    ] {
        std::fs::write(root.path().join(name), text).unwrap();
    }
    let harness = ToolHarness::filesystem(FileSystemToolsConfig::new(root.path())).await;
    let query = json!({"patterns":["**/*.{rs,txt}"],"maxResults":1});
    let first = harness
        .call("owner", "search_glob", query.clone())
        .await
        .unwrap();
    assert_eq!(first["paths"], json!(["a.rs"]));
    let cursor = first["nextCursor"].clone();
    assert!(cursor.is_string());
    // Deleting a file after the first page must not shift or duplicate later rows.
    std::fs::remove_file(root.path().join("src/b.rs")).unwrap();
    let mut next = query.clone();
    next["cursor"] = cursor.clone();
    let second = harness
        .call("owner", "search_glob", next.clone())
        .await
        .unwrap();
    assert_eq!(second["paths"], json!(["src/b.rs"]));
    assert!(
        harness
            .call("foreign", "search_glob", next.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("another query/session")
    );
    next["patterns"] = json!(["*.txt"]);
    assert!(harness.call("owner", "search_glob", next).await.is_err());
    let files = harness
        .call(
            "owner",
            "search_grep",
            json!({"pattern":"a.b","fixedStrings":true,"mode":"files","exclude":["src/**"]}),
        )
        .await
        .unwrap();
    assert_eq!(files["paths"], json!(["a.rs"]));
    let counts = harness
        .call(
            "owner",
            "search_grep",
            json!({"pattern":"a.b","fixedStrings":true,"mode":"count","exclude":["src/**"]}),
        )
        .await
        .unwrap();
    assert_eq!(counts["counts"], json!([{"path":"a.rs","count":1}]));
    let all = harness
        .call(
            "owner",
            "search_glob",
            json!({"patterns":["**/*.rs"],"respectGitIgnore":false,"includeHidden":true}),
        )
        .await
        .unwrap();
    assert!(
        all["paths"]
            .as_array()
            .unwrap()
            .contains(&json!("ignored.rs"))
    );
    assert!(
        all["paths"]
            .as_array()
            .unwrap()
            .contains(&json!(".hidden.rs"))
    );
    let excluded_dir = harness
        .call(
            "owner",
            "search_glob",
            json!({"patterns":["**/*.{rs,txt}"],"exclude":["src"]}),
        )
        .await
        .unwrap();
    assert_eq!(excluded_dir["paths"], json!(["a.rs"]));
    let dirs = harness
        .call(
            "owner",
            "search_path",
            json!({"patterns":["s[rt]c"],"entryType":"directory"}),
        )
        .await
        .unwrap();
    assert_eq!(dirs["entries"][0]["path"], "src");
    assert!(harness.agent.framework().close().await.is_success());
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_symlinks_are_rejected_before_creation() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path().join("missing"), root.path().join("link")).unwrap();
    let error = invoke_error(
        write_filesystem_pack(FileSystemToolsConfig::new(root.path())),
        "fs_write_text",
        json!({"path":"link","content":"no"}),
    )
    .await;
    assert!(error.contains("symbolic links"));
    assert!(!outside.path().join("missing").exists());
}

#[tokio::test]
async fn git_rename_moves_content_and_requires_both_path_versions() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("old.txt"), "keep\n").unwrap();
    let harness = ToolHarness::filesystem(FileSystemToolsConfig::new(root.path())).await;
    let read = harness
        .call("owner", "fs_read_text", json!({"path":"old.txt"}))
        .await
        .unwrap();
    let patch = "diff --git a/old.txt b/sub/new.txt\nsimilarity index 100%\nrename from old.txt\nrename to sub/new.txt\n";
    let args = json!({"format":"git","patch":patch,"expectedVersions":{"old.txt":read["version"],"sub/new.txt":"absent"}});
    let result = harness
        .call("owner", "fs_apply_unified_patch", args)
        .await
        .unwrap();
    assert_eq!(result["changes"].as_array().unwrap().len(), 2);
    assert!(!root.path().join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("sub/new.txt")).unwrap(),
        "keep\n"
    );
    assert!(harness.agent.framework().close().await.is_success());
}

#[cfg(unix)]
#[tokio::test]
async fn pty_capability_closes_sessions_shared_with_host_adapters() {
    use eliteforge_agent_capability_tools::{PtyCommand, PtyService, pty_pack};
    let service = PtyService::default();
    let agent = builder(InvokeToolLoop {
        call_name: "unused",
        arguments: Value::Null,
        output: Arc::new(Mutex::new(None)),
    })
    .capability_pack(pty_pack(service.clone()))
    .build()
    .await
    .expect("PTY capability should assemble");
    let mut command = PtyCommand::new("/bin/sh");
    command.args(["-c", "sleep 60"]);
    command.cwd("/tmp");
    let (session, _) = service
        .start_stream("host".into(), command.clone(), 80, 24)
        .unwrap();
    assert!(
        service
            .read("host", &session.terminal_id, 0)
            .unwrap()
            .running
    );
    assert!(agent.framework().close().await.is_success());
    assert!(service.read("host", &session.terminal_id, 0).is_err());
    assert!(
        service
            .start_stream("host".into(), command, 80, 24)
            .is_err()
    );
}
